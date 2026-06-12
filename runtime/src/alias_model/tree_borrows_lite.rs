use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use crate::{
    allocs, append_location_if_enabled, boundary_survivor_tags_for_callee,
    bounds_len_bytes_or_zero, bounds_len_is_precise_empty, find_alloc_containing, rz_sb_suppressed,
    rz_violation, tag_pruning, tag_store, tags, PtrKind, TagMeta,
};

use super::{AliasAccessKind, AliasModel};

pub(crate) struct TreeBorrowsLiteModel;
// Source used a projected helper parent, so parent shape is part of the TB-lite decision.
const TB_LITE_HINT_PROJECTED_HELPER_PARENT: u8 = 0b0000_0100;
// Returned by-value carrier refs may reroot a caller-visible same-slot family.
const TB_LITE_HINT_RETURNED_CARRIER_REROOT: u8 = 0b0000_1000;
// Example: `q = p.add(1)` stays in `p`'s TB family; it is not a new raw authority.
const TB_LITE_HINT_RAW_REUSE_PARENT_FAMILY: u8 = 0b0001_0000;
const TB_RANGE_INDEX_BUCKET_SIZE: usize = 64;
const TB_RANGE_INDEX_MAX_BUCKETS_PER_RANGE: usize = 256;
const TB_RANGE_INDEX_WIDE_BUCKET: usize = usize::MAX;
// The index path has HashMap and dedupe overhead; use it only for clear scan cuts.
const TB_RANGE_INDEX_MIN_SCAN_REDUCTION: usize = 8;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BorrowKind {
    Shared,
    Unique,
    RawConst,
    RawMut,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TbPerm {
    Reserved { conflicted: bool },
    Active,
    Frozen,
    ShadowedLocal,
    Disabled,
}

#[derive(Clone, Debug)]
struct TbNode {
    tag: u64,
    parent: u64,
    alloc_epoch: u64,
    kind: BorrowKind,
    perm: TbPerm,
    // Default permission for same-allocation bytes not yet covered by this node's ranges.
    lazy_perm: TbPerm,
    // Last allocation read epoch folded into `lazy_perm`.
    lazy_read_epoch: u64,
    start: usize,
    len: usize,
    extra_ranges: Vec<(usize, usize)>,
    alive: bool,
    protected: bool,
    protector_shadow_depth: u32,
    poisoned_by_protector_end: bool,
}

#[derive(Default)]
struct TbAllocState {
    nodes: HashMap<u64, TbNode>,
    children: HashMap<u64, Vec<u64>>,
    range_index: HashMap<usize, Vec<u64>>,
    protected_tags: Vec<u64>,
    foreign_read_epoch: u64,
}

impl TbAllocState {
    fn insert_node(&mut self, node: TbNode) {
        if let Some(old) = self.nodes.insert(node.tag, node.clone()) {
            self.unlink_child(old.parent, old.tag);
            self.unindex_node_ranges(&old);
            self.unindex_protected_tag(old.tag);
        }
        self.link_child(node.parent, node.tag);
        self.index_node_ranges(&node);
        if node.protected {
            self.index_protected_tag(node.tag);
        }
    }

    fn remove_node(&mut self, tag: u64) -> Option<TbNode> {
        let node = self.nodes.remove(&tag)?;
        self.unlink_child(node.parent, tag);
        self.children.remove(&tag);
        self.unindex_node_ranges(&node);
        self.unindex_protected_tag(tag);
        Some(node)
    }

    fn link_child(&mut self, parent: u64, child: u64) {
        if parent == 0 || child == 0 {
            return;
        }
        let children = self.children.entry(parent).or_default();
        if !children.iter().any(|tag| *tag == child) {
            children.push(child);
        }
    }

    fn unlink_child(&mut self, parent: u64, child: u64) {
        if parent == 0 || child == 0 {
            return;
        }
        let Some(children) = self.children.get_mut(&parent) else {
            return;
        };
        children.retain(|tag| *tag != child);
        if children.is_empty() {
            self.children.remove(&parent);
        }
    }

    fn index_node_ranges(&mut self, node: &TbNode) {
        self.index_range(node.tag, node.start, node.len);
        for (start, len) in &node.extra_ranges {
            self.index_range(node.tag, *start, *len);
        }
    }

    fn unindex_node_ranges(&mut self, node: &TbNode) {
        self.unindex_range(node.tag, node.start, node.len);
        for (start, len) in &node.extra_ranges {
            self.unindex_range(node.tag, *start, *len);
        }
    }

    fn index_range(&mut self, tag: u64, start: usize, len: usize) {
        for key in tb_range_index_keys(start, len) {
            let tags = self.range_index.entry(key).or_default();
            if !tags.iter().any(|indexed| *indexed == tag) {
                tags.push(tag);
            }
        }
    }

    fn unindex_range(&mut self, tag: u64, start: usize, len: usize) {
        for key in tb_range_index_keys(start, len) {
            let Some(tags) = self.range_index.get_mut(&key) else {
                continue;
            };
            tags.retain(|indexed| *indexed != tag);
            if tags.is_empty() {
                self.range_index.remove(&key);
            }
        }
    }

    fn overlapping_indexed_tags(&self, start: usize, len: usize) -> Vec<u64> {
        let mut tags = Vec::new();
        for key in tb_range_index_keys(start, len)
            .into_iter()
            .chain(std::iter::once(TB_RANGE_INDEX_WIDE_BUCKET))
        {
            let Some(indexed) = self.range_index.get(&key) else {
                continue;
            };
            for tag in indexed {
                if !tags.iter().any(|seen| seen == tag) {
                    tags.push(*tag);
                }
            }
        }
        tags
    }

    fn index_protected_tag(&mut self, tag: u64) {
        if tag != 0 && !self.protected_tags.iter().any(|indexed| *indexed == tag) {
            self.protected_tags.push(tag);
        }
    }

    fn unindex_protected_tag(&mut self, tag: u64) {
        self.protected_tags.retain(|indexed| *indexed != tag);
    }
}

#[derive(Copy, Clone, Debug)]
struct TbCompactedNode {
    tag: u64,
    base: usize,
    alloc_epoch: u64,
    kind: BorrowKind,
    start: usize,
    len: usize,
}

struct TbProtectorFrame {
    thread_id: ThreadId,
    callee_id: u64,
    pending_parent_tags: Vec<u64>,
    protected_tags: Vec<u64>,
    pending_inplace_parent_tags: Vec<(u64, usize)>,
    inplace_protected_tags: Vec<u64>,
}

static TB_STATE: OnceLock<Mutex<HashMap<usize, TbAllocState>>> = OnceLock::new();
static TB_PROTECTOR_FRAMES: OnceLock<Mutex<Vec<TbProtectorFrame>>> = OnceLock::new();
static TB_COMPACTED_INVALIDATED: OnceLock<Mutex<HashMap<u64, TbCompactedNode>>> = OnceLock::new();

fn tb_state() -> &'static Mutex<HashMap<usize, TbAllocState>> {
    TB_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tb_protector_frames() -> &'static Mutex<Vec<TbProtectorFrame>> {
    TB_PROTECTOR_FRAMES.get_or_init(|| Mutex::new(Vec::new()))
}

fn tb_compacted_invalidated() -> &'static Mutex<HashMap<u64, TbCompactedNode>> {
    TB_COMPACTED_INVALIDATED.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn tb_protector_active(node: &TbNode) -> bool {
    node.protected && node.protector_shadow_depth == 0
}

#[inline]
fn rz_tb_lite_enabled() -> bool {
    std::env::var("RZ_TB_LITE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_tb_dump_enabled() -> bool {
    std::env::var("RZ_TB_DUMP")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_tb_trace_enabled() -> bool {
    std::env::var("RZ_TB_TRACE")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_tb_compact_invalidated_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RZ_TB_COMPACT_INVALIDATED_TAGS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

impl AliasModel for TreeBorrowsLiteModel {
    fn name(&self) -> &'static str {
        "tb_lite"
    }

    fn violation_kind(&self) -> &'static str {
        "TREE_BORROWS_VIOLATION"
    }

    fn on_alloc_state_change(&self, base_addr: usize, new_live: bool) {
        if !new_live && rz_tb_lite_enabled() {
            tb_lite_check_protected_dealloc(base_addr);
            tb_state().lock().unwrap().remove(&base_addr);
            tb_compacted_invalidated()
                .lock()
                .unwrap()
                .retain(|_, node| node.base != base_addr);
        }
    }

    fn validate_ref_creation(
        &self,
        pointee_addr: usize,
        new_kind: PtrKind,
        parent_tag: u64,
        alias_exempt: bool,
        bounds_len: usize,
    ) -> Option<String> {
        tb_lite_validate_ref_creation(pointee_addr, new_kind, parent_tag, alias_exempt, bounds_len)
    }

    fn on_tag_created(&self, tag: u64, tmeta: &TagMeta) {
        tb_lite_on_tag_created(tag, tmeta);
    }

    fn on_tag_killed(&self, tag: u64) {
        tb_lite_on_tag_killed(tag);
    }

    fn on_call_arg_taken(&self, callee_id: u64, parent_tag: u64) {
        tb_lite_on_call_arg_taken(callee_id, parent_tag);
    }

    fn on_call_arg_anchor_taken(&self, callee_id: u64, parent_tag: u64) {
        tb_lite_on_call_arg_anchor_taken(callee_id, parent_tag);
    }

    fn on_call_arg_inplace_alias(&self, callee_id: u64, parent_tag: u64, addr: usize) {
        tb_lite_on_call_arg_inplace_alias(callee_id, parent_tag, addr);
    }

    fn on_call_exit(&self, callee_id: u64) {
        tb_lite_on_call_exit(callee_id);
    }

    fn validate_ref_mut_boundary_retag(
        &self,
        tag: u64,
        tmeta: &TagMeta,
        addr: usize,
        size: usize,
    ) -> Option<String> {
        tb_lite_validate_ref_mut_boundary_retag(tag, tmeta, addr, size)
    }

    fn validate_call_arg_boundary_parent(
        &self,
        tag: u64,
        tmeta: &TagMeta,
        addr: usize,
        size: usize,
    ) -> Option<String> {
        tb_lite_validate_call_arg_boundary_parent(tag, tmeta, addr, size)
    }

    fn find_ref_ancestor_tag(&self, tmap: &HashMap<u64, TagMeta>, tag: u64) -> Option<u64> {
        tb_lite_find_ref_ancestor_tag(tmap, tag)
    }

    fn can_recover_parent_tag(&self, tag: u64) -> bool {
        tb_lite_can_recover_parent_tag(tag)
    }

    fn canonicalize_mut_arg_ret_tag(&self, tag: u64, addr: usize) -> u64 {
        tb_lite_canonicalize_mut_arg_ret_tag(tag, addr)
    }

    fn on_mut_arg_ret_export(&self, tag: u64, addr: usize) {
        tb_lite_on_mut_arg_ret_export(tag, addr);
    }

    fn on_ret_export(&self, tag: u64, addr: usize) {
        tb_lite_on_return_export(tag, addr);
    }

    fn check_access(
        &self,
        sb_tag: u64,
        orig_tag: u64,
        tmeta: &TagMeta,
        addr: usize,
        size: usize,
        access: AliasAccessKind,
    ) -> Option<String> {
        tb_lite_check(sb_tag, orig_tag, tmeta, addr, size, access)
    }
}

/// Record that the current callee will materialize a protected child from `parent_tag`.
fn tb_lite_on_call_arg_taken(callee_id: u64, parent_tag: u64) {
    if !rz_tb_lite_enabled() || parent_tag == 0 {
        return;
    }
    let thread_id = std::thread::current().id();
    let mut frames = tb_protector_frames().lock().unwrap();
    match frames.last_mut() {
        Some(top) if top.thread_id == thread_id && top.callee_id == callee_id => {
            top.pending_parent_tags.push(parent_tag);
        }
        _ => {
            frames.push(TbProtectorFrame {
                thread_id,
                callee_id,
                pending_parent_tags: vec![parent_tag],
                protected_tags: Vec::new(),
                pending_inplace_parent_tags: Vec::new(),
                inplace_protected_tags: Vec::new(),
            });
        }
    }
}

/// Aggregate carriers feed the same protector pipeline as plain pointer args, but the protected
/// child is created later from the imported anchor rather than directly from the ABI argument.
fn tb_lite_on_call_arg_anchor_taken(callee_id: u64, parent_tag: u64) {
    if !rz_tb_lite_enabled() || parent_tag == 0 {
        return;
    }

    // For aggregate/non-pointer carriers we still want callee-side protector semantics, but the
    // protected node must be the immediate ref child materialized in the callee, not the caller's
    // raw/parent tag. Protecting the parent directly incorrectly makes sibling raw accesses look
    // like descendant/self accesses and hides the conflict that Tree Borrows should report.
    tb_lite_on_call_arg_taken(callee_id, parent_tag);
}

/// Track exact-slot by-value/in-place aliases so a raw child created in the callee can inherit
/// the same call-arg protector semantics as its paired reference argument.
fn tb_lite_on_call_arg_inplace_alias(callee_id: u64, parent_tag: u64, addr: usize) {
    if !rz_tb_lite_enabled() || parent_tag == 0 || addr == 0 {
        return;
    }
    let thread_id = std::thread::current().id();
    let mut frames = tb_protector_frames().lock().unwrap();
    match frames.last_mut() {
        Some(top) if top.thread_id == thread_id && top.callee_id == callee_id => {
            top.pending_inplace_parent_tags.push((parent_tag, addr));
        }
        _ => {
            frames.push(TbProtectorFrame {
                thread_id,
                callee_id,
                pending_parent_tags: Vec::new(),
                protected_tags: Vec::new(),
                pending_inplace_parent_tags: vec![(parent_tag, addr)],
                inplace_protected_tags: Vec::new(),
            });
        }
    }
}

/// End the current call-frame protector scope.
///
/// Protected children created for this callee are released, any same-slot ancestor protectors
/// shadowed by nested `&mut self` calls are restored, and non-returned protected unique tags are
/// retired at protector end.
fn tb_lite_on_call_exit(callee_id: u64) {
    if !rz_tb_lite_enabled() {
        return;
    }
    let thread_id = std::thread::current().id();

    let popped = {
        let mut frames = tb_protector_frames().lock().unwrap();
        frames
            .iter()
            .rposition(|f| f.thread_id == thread_id && f.callee_id == callee_id)
            .map(|idx| frames.remove(idx))
    };

    let Some(frame) = popped else {
        return;
    };
    if frame.protected_tags.is_empty() {
        return;
    }

    let returned_tags = boundary_survivor_tags_for_callee(callee_id);
    let tmap = tags().lock().unwrap();
    let mut all = tb_state().lock().unwrap();
    for tag in frame.protected_tags {
        let Some(tmeta) = tmap.get(&tag) else {
            continue;
        };
        let base = tb_base_for_addr(tmeta.pointee_addr);
        let Some(tree) = all.get_mut(&base) else {
            continue;
        };
        let active_protected_unique = tree
            .nodes
            .get(&tag)
            .filter(|node| {
                matches!(node.kind, BorrowKind::Unique)
                    && matches!(node.perm, TbPerm::Active)
                    && tb_protector_active(node)
            })
            .cloned();
        if let Some(protected_node) = active_protected_unique {
            let descendant_tags = tb_descendant_tags(tree, tag);
            for descendant in descendant_tags {
                // A returned descendant is caller-visible; only retire non-returned call locals.
                if tb_returned_lineage_needs_tag(tree, descendant, &returned_tags) {
                    continue;
                }
                if let Some(node) = tree.nodes.get_mut(&descendant) {
                    tb_disable_node_for_protector_end(node);
                }
            }
            let foreign_overlap_tags: Vec<u64> = tree
                .nodes
                .values()
                .filter(|n| n.tag != tag)
                .filter(|n| tb_is_live_node(n))
                .filter(|n| {
                    tb_node_overlaps(n, protected_node.start, protected_node.len)
                        || protected_node
                            .extra_ranges
                            .iter()
                            .any(|(start, len)| tb_node_overlaps(n, *start, *len))
                })
                .filter(|n| {
                    !tb_is_ancestor(&tree.nodes, n.tag, tag)
                        && !tb_is_ancestor(&tree.nodes, tag, n.tag)
                })
                .map(|n| n.tag)
                .collect();
            for foreign in foreign_overlap_tags {
                if let Some(node) = tree.nodes.get_mut(&foreign) {
                    tb_disable_node_for_protector_end(node);
                }
            }
        }
        let returned_descendant = returned_tags
            .iter()
            .copied()
            .any(|ret_tag| ret_tag != tag && tb_is_ancestor(&tree.nodes, tag, ret_tag));
        // A shadowed protected parent is an obsolete local handle while a nested same-slot
        // `&mut` child is active. Keep it as lineage if that child family survives the call.
        let shadowed_live_descendant = tree.nodes.get(&tag).is_some_and(|node| {
            matches!(node.kind, BorrowKind::Unique)
                && node.protector_shadow_depth != 0
                && tb_has_live_same_slot_unique_descendant(tree, tag, node.start, node.len)
        });
        let read_epoch = tree.foreign_read_epoch;
        if let Some(node) = tree.nodes.get_mut(&tag) {
            node.protected = false;
            if !returned_tags.contains(&tag) {
                if (returned_descendant || shadowed_live_descendant)
                    && matches!(node.kind, BorrowKind::Unique)
                {
                    tb_shadow_local_node(node, read_epoch);
                } else if matches!(node.kind, BorrowKind::Unique)
                    && !(matches!(node.perm, TbPerm::Reserved { .. }) && returned_descendant)
                {
                    tb_disable_node_for_protector_end(node);
                }
            }
        }
        tb_unshadow_same_slot_protected_unique_ancestors(
            tree,
            tag,
            tmeta.pointee_addr,
            tb_node_len_for_meta(tmeta),
        );
    }
    for tag in frame.inplace_protected_tags {
        let Some(tmeta) = tmap.get(&tag) else {
            continue;
        };
        let base = tb_base_for_addr(tmeta.pointee_addr);
        let Some(tree) = all.get_mut(&base) else {
            continue;
        };
        let descendant_tags = tb_descendant_tags(tree, tag);
        for descendant in descendant_tags {
            // A returned descendant is caller-visible; only retire non-returned call locals.
            if tb_returned_lineage_needs_tag(tree, descendant, &returned_tags) {
                continue;
            }
            if let Some(node) = tree.nodes.get_mut(&descendant) {
                tb_disable_node_for_protector_end(node);
            }
        }
        if let Some(node) = tree.nodes.get_mut(&tag) {
            tb_disable_node_for_protector_end(node);
        }
    }
}

/// Check a returned `&mut` as a TB retag, without pretending the caller wrote through it.
///
/// Miri rejects a mutable reference that is already frozen or disabled at the return boundary.
/// We report that as a boundary READ because no concrete caller write has happened yet.
fn tb_lite_validate_ref_mut_boundary_retag(
    tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> Option<String> {
    if !rz_tb_lite_enabled()
        || tmeta.alias_exempt
        || rz_sb_suppressed()
        || !matches!(tmeta.kind, PtrKind::RefMut)
    {
        return None;
    }

    let base = tb_base_for_addr(addr);
    let all = tb_state().lock().unwrap();
    let tree = all.get(&base)?;
    let node = tree.nodes.get(&tag)?;
    if tmeta.alloc_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != tmeta.alloc_epoch {
        return None;
    }

    let dump = if rz_tb_dump_enabled() {
        tb_dump(tree, tag, addr, size, AliasAccessKind::Read)
    } else {
        String::new()
    };
    let reason = if !tb_is_live_node(node) {
        Some("TB_LITE_INVALIDATED")
    } else {
        match node.perm {
            TbPerm::Reserved { conflicted: true } if tb_protector_active(node) => {
                Some("TB_LITE_2PHASE_CONFLICT")
            }
            TbPerm::Reserved { .. } | TbPerm::Active => None,
            TbPerm::Frozen => Some("TB_LITE_FROZEN_WRITE"),
            TbPerm::ShadowedLocal => Some("TB_LITE_SHADOWED_LOCAL"),
            TbPerm::Disabled => Some("TB_LITE_DISABLED_WRITE"),
        }
    }?;

    let mut msg = format!(
        "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason={}",
        tag, addr, size, tmeta.kind, reason
    );
    msg.push_str(&dump);
    Some(msg)
}

/// Check that a call argument can be used as the parent for callee-entry retagging.
///
/// This deliberately does not perform the full TB read transition: transporting a ref argument is
/// not a pointee load. Actual retags and reads/writes in the callee still go through the normal
/// creation/access paths.
fn tb_lite_validate_call_arg_boundary_parent(
    tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> Option<String> {
    if !rz_tb_lite_enabled()
        || tmeta.alias_exempt
        || rz_sb_suppressed()
        || !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
    {
        return None;
    }

    if matches!(tmeta.kind, PtrKind::RefMut)
        && tmeta.parent != 0
        && tag_store::get(tmeta.parent)
            .as_ref()
            .is_some_and(|parent| matches!(parent.kind, PtrKind::RawConst | PtrKind::RawMut))
    {
        // A raw-to-`&mut` call argument is not just exporting an existing ref family: this boundary
        // materializes mutable reference authority from raw provenance. Keep the TB read-retag
        // transition here so protector/lazy-interior-mutability cases observe the same state change
        // they would get from a real retag, while ordinary ref-rooted arguments use the cheap path.
        return tb_lite_check(tag, tag, tmeta, addr, size, AliasAccessKind::Read);
    }

    if let Some(compacted) = tb_compacted_invalidated_hit(&[tag]) {
        return Some(tb_lite_invalidated_tombstone_msg(
            compacted,
            tmeta,
            addr,
            size,
            AliasAccessKind::Read,
        ));
    }

    let base = tb_base_for_addr(addr);
    let all = tb_state().lock().unwrap();
    let Some(tree) = all.get(&base) else {
        if let Some(compacted) = tb_compacted_invalidated_hit(&[tag]) {
            return Some(tb_lite_invalidated_tombstone_msg(
                compacted,
                tmeta,
                addr,
                size,
                AliasAccessKind::Read,
            ));
        }
        return None;
    };

    let Some(node) = tree.nodes.get(&tag) else {
        if let Some(compacted) = tb_compacted_invalidated_hit(&[tag]) {
            return Some(tb_lite_invalidated_tombstone_msg(
                compacted,
                tmeta,
                addr,
                size,
                AliasAccessKind::Read,
            ));
        }
        // Missing TB metadata is best-effort loss, not proof of a bad program.
        return None;
    };

    if tmeta.alloc_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != tmeta.alloc_epoch {
        return None;
    }

    let dump = if rz_tb_dump_enabled() {
        tb_dump(tree, tag, addr, size, AliasAccessKind::Read)
    } else {
        String::new()
    };
    if tb_lite_inplace_protected_tag(tag) {
        let mut msg = format!(
            "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INPLACE_CALL_ARG",
            tag, addr, size, tmeta.kind
        );
        msg.push_str(&dump);
        return Some(msg);
    }
    if !tb_is_live_node(node) {
        let mut msg = format!(
            "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INVALIDATED",
            tag, addr, size, tmeta.kind
        );
        msg.push_str(&dump);
        return Some(msg);
    }

    if matches!(tmeta.kind, PtrKind::RefMut) {
        let lineage = tb_collect_lineage(&tree.nodes, tag);
        for ancestor_tag in lineage.iter().copied().skip(1) {
            let Some(ancestor) = tree.nodes.get(&ancestor_tag) else {
                continue;
            };
            if matches!(ancestor.kind, BorrowKind::Unique)
                && matches!(ancestor.perm, TbPerm::Disabled)
            {
                let mut msg = format!(
                    "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_DISABLED_ANCESTOR ancestor_tag={}",
                    tag, addr, size, tmeta.kind, ancestor.tag
                );
                msg.push_str(&dump);
                return Some(msg);
            }
        }
    }

    None
}

fn tb_lite_check_protected_dealloc(base_addr: usize) {
    let is_stack = {
        let amap = allocs().lock().unwrap();
        amap.get(&base_addr).map_or(false, |m| m.is_stack)
    };
    if is_stack {
        return;
    }

    let protected = {
        let all = tb_state().lock().unwrap();
        let Some(tree) = all.get(&base_addr) else {
            return;
        };
        tree.nodes
            .values()
            .find(|n| {
                tb_is_live_node(n) && tb_protector_active(n) && matches!(n.kind, BorrowKind::Unique)
            })
            .map(|n| (n.tag, n.kind))
    };

    if let Some((tag, kind)) = protected {
        rz_violation(
            "TREE_BORROWS_VIOLATION",
            append_location_if_enabled(
                format!(
                    "DEALLOC base=0x{:x}\nreason=TB_LITE_PROTECTOR_DEALLOC tag={} kind={:?}",
                    base_addr, tag, kind
                ),
                "RZ_LOG_LOC",
            ),
        );
    }
}

fn tb_lite_validate_ref_creation(
    pointee_addr: usize,
    new_kind: PtrKind,
    parent_tag: u64,
    alias_exempt: bool,
    bounds_len: usize,
) -> Option<String> {
    if !rz_tb_lite_enabled() || alias_exempt || rz_sb_suppressed() {
        return None;
    }
    if !matches!(new_kind, PtrKind::RefShared | PtrKind::RefMut) {
        return None;
    }

    if bounds_len_is_precise_empty(bounds_len) {
        // Empty slice/str views are metadata transport, not a byte read through the parent.
        return None;
    }

    let new_len = tb_effective_access_len(bounds_len_bytes_or_zero(bounds_len));
    let new_end = pointee_addr.saturating_add(new_len);
    let base = tb_base_for_addr(pointee_addr);
    let (parent_ref, parent_epoch, parent_tag_epoch) = if parent_tag != 0 {
        let tmap = tags().lock().unwrap();
        let pref = tb_lite_find_ref_ancestor_tag(&tmap, parent_tag);
        let pep = pref
            .and_then(|t| tmap.get(&t).map(|m| m.alloc_epoch))
            .unwrap_or(0);
        let ptag_epoch = tmap.get(&parent_tag).map(|m| m.alloc_epoch).unwrap_or(0);
        (pref, pep, ptag_epoch)
    } else {
        (None, 0, 0)
    };

    let state = tb_state().lock().unwrap();
    let Some(tree) = state.get(&base) else {
        return None;
    };
    if let Some(parent_node) = tree.nodes.get(&parent_tag) {
        if (parent_tag_epoch == 0
            || parent_node.alloc_epoch == 0
            || parent_node.alloc_epoch == parent_tag_epoch)
            && !tb_is_live_node(parent_node)
            && parent_node.poisoned_by_protector_end
        {
            let access_name = match new_kind {
                PtrKind::RefMut => "WRITE",
                PtrKind::RefShared => "READ",
                _ => "READ",
            };
            return Some(format!(
                "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INVALID_PARENT_REF_CREATE create_kind={:?} parent_perm={:?}",
                access_name,
                parent_tag,
                pointee_addr,
                new_len,
                parent_node.kind,
                new_kind,
                parent_node.perm,
            ));
        }
    }

    let Some(parent_ref) = parent_ref else {
        // Without a ref ancestor we cannot reason about branch lineage reliably.
        // Skip creation-time overlap rejection and let access-time invalidation/checks decide.
        return None;
    };

    if !matches!(new_kind, PtrKind::RefMut) {
        // Shared reborrows still validate that the immediate parent tag itself is alive (above),
        // but they defer overlap/freeze behavior to access-time transitions.
        return None;
    }

    if !tree.nodes.contains_key(&parent_ref) {
        // Parent lineage is not materialized in TB state (best-effort metadata loss).
        // Be conservative and defer to access-time checks rather than rejecting now.
        return None;
    }

    None
}

#[inline]
fn tb_lite_should_defer_raw_family(tmeta: &TagMeta, kind: BorrowKind) -> bool {
    matches!(kind, BorrowKind::RawConst | BorrowKind::RawMut)
        && tmeta.parent != 0
        && (tmeta.lineage_hint & TB_LITE_HINT_RAW_REUSE_PARENT_FAMILY) != 0
}

// A raw transport helper is only a value-level step such as `q = p.add(1)`.
// When creating a real ref from it, skip the helper and use the underlying
// authority family. For raw-to-raw children, keep covering helpers so real raw
// accesses still observe their TB state.
fn tb_lite_raw_transport_parent_skippable(
    tmap: &HashMap<u64, TagMeta>,
    nodes: &HashMap<u64, TbNode>,
    tag: u64,
    child_start: usize,
    child_len: usize,
    skip_even_when_covering: bool,
) -> bool {
    if child_len == 0 {
        return false;
    }
    let Some(node) = nodes.get(&tag) else {
        return false;
    };
    matches!(node.kind, BorrowKind::RawConst | BorrowKind::RawMut)
        && !tb_protector_active(node)
        && (skip_even_when_covering || !tb_node_covers(node, child_start, child_len))
        && tb_lite_raw_transport_family_marked(tmap, tag, node.kind)
}

fn tb_lite_resolve_parent_for_new_node(
    tree: &TbAllocState,
    tmeta: &TagMeta,
    kind: BorrowKind,
) -> u64 {
    if tmeta.parent == 0 {
        return 0;
    }
    let tmap = tags().lock().unwrap();
    let projected_helper_ref_parent = matches!(kind, BorrowKind::Shared | BorrowKind::Unique)
        && (tmeta.lineage_hint & TB_LITE_HINT_PROJECTED_HELPER_PARENT) != 0
        && tmap.get(&tmeta.parent).is_some_and(|meta| {
            meta.parent == 0 && matches!(meta.kind, PtrKind::RawConst | PtrKind::RawMut)
        });
    let effective_parent = if projected_helper_ref_parent {
        tb_lite_find_ref_ancestor_tag(&tmap, tmeta.parent).unwrap_or(0)
    } else {
        tmeta.parent
    };
    let exact_new_len = tb_node_len_for_meta(tmeta);
    let effective_parent = if matches!(kind, BorrowKind::Shared | BorrowKind::Unique)
        && effective_parent != 0
        && tree.nodes.get(&effective_parent).is_some_and(|node| {
            !tb_is_live_node(node)
                && node.parent == 0
                && matches!(node.kind, BorrowKind::Shared | BorrowKind::Unique)
                && node.start == tmeta.pointee_addr
                && node.len == exact_new_len
        }) {
        0
    } else {
        effective_parent
    };
    match kind {
        BorrowKind::Shared | BorrowKind::Unique => {
            tb_lite_find_materialized_ref_ancestor_tag(&tmap, &tree.nodes, effective_parent)
                .or_else(|| {
                    tb_lite_find_ref_ancestor_tag(&tmap, effective_parent)
                        .filter(|tag| tree.nodes.contains_key(tag))
                })
                .or_else(|| {
                    tb_lite_find_materialized_authority_ancestor_tag(
                        &tmap,
                        &tree.nodes,
                        effective_parent,
                        tmeta.pointee_addr,
                        exact_new_len,
                        true,
                    )
                })
                .unwrap_or(0)
        }
        BorrowKind::RawConst | BorrowKind::RawMut => {
            tb_lite_find_materialized_ref_ancestor_tag(&tmap, &tree.nodes, effective_parent)
                .or_else(|| {
                    tb_lite_find_ref_ancestor_tag(&tmap, effective_parent)
                        .filter(|tag| tree.nodes.contains_key(tag))
                })
                .or_else(|| {
                    tb_lite_find_materialized_authority_ancestor_tag(
                        &tmap,
                        &tree.nodes,
                        effective_parent,
                        tmeta.pointee_addr,
                        exact_new_len,
                        false,
                    )
                })
                .unwrap_or(0)
        }
    }
}

fn tb_lite_insert_tag_node(
    tree: &mut TbAllocState,
    tag: u64,
    tmeta: &TagMeta,
    kind: BorrowKind,
) -> TbNode {
    let perm = match kind {
        BorrowKind::Unique => TbPerm::Reserved { conflicted: false },
        BorrowKind::RawMut => TbPerm::Active,
        BorrowKind::Shared | BorrowKind::RawConst => TbPerm::Frozen,
    };
    let parent = tb_lite_resolve_parent_for_new_node(tree, tmeta, kind);
    let protected = tb_lite_mark_protected_if_pending(tag, parent, kind);
    tb_lite_mark_inplace_protected_if_pending(tag, parent, tmeta.parent, tmeta.pointee_addr, kind);
    let node = TbNode {
        tag,
        parent,
        alloc_epoch: tmeta.alloc_epoch,
        kind,
        perm,
        lazy_perm: perm,
        lazy_read_epoch: tree.foreign_read_epoch,
        start: tmeta.pointee_addr,
        len: tb_node_len_for_meta(tmeta),
        extra_ranges: Vec::new(),
        alive: true,
        protected,
        protector_shadow_depth: 0,
        poisoned_by_protector_end: false,
    };
    tree.insert_node(node.clone());
    node
}

fn tb_lite_materialize_deferred_raw_node(
    tree: &mut TbAllocState,
    tag: u64,
    tmeta: &TagMeta,
) -> Option<TbNode> {
    let kind = match tmeta.kind {
        PtrKind::RawConst => BorrowKind::RawConst,
        PtrKind::RawMut => BorrowKind::RawMut,
        _ => return None,
    };
    if !tb_lite_should_defer_raw_family(tmeta, kind) {
        return None;
    }
    if let Some(existing) = tree.nodes.get(&tag).cloned() {
        return Some(existing);
    }
    Some(tb_lite_insert_tag_node(tree, tag, tmeta, kind))
}

/// Retire a temporary raw parent used only to create a real `&mut`.
///
/// Example: `r: &mut T` becomes `p: *mut T`, then `child: &mut T` is made from
/// `p`, and the write happens through `child`. The raw pointer `p` is just a
/// bridge; it should not stay in the tree as a frozen node that blocks the
/// `child` write. If the raw parent was deferred, create its node first, then
/// disable it unless that raw parent or its parent family is protected.
fn tb_lite_materialize_and_disable_deferred_raw_parent_of_ref_write(
    tree: &mut TbAllocState,
    tmeta: &TagMeta,
) {
    if !matches!(tmeta.kind, PtrKind::RefMut) || tmeta.parent == 0 {
        return;
    }
    let Some(parent_meta) = tag_store::get(tmeta.parent) else {
        return;
    };
    if !matches!(parent_meta.kind, PtrKind::RawConst | PtrKind::RawMut)
        || (parent_meta.lineage_hint & TB_LITE_HINT_RAW_REUSE_PARENT_FAMILY) == 0
    {
        return;
    }
    if let Some(raw_node) = tb_lite_materialize_deferred_raw_node(tree, tmeta.parent, &parent_meta)
    {
        let protected_raw_parent = tree
            .nodes
            .get(&tmeta.parent)
            .is_some_and(tb_protector_active);
        let protected_parent_family = tree
            .nodes
            .get(&raw_node.parent)
            .is_some_and(tb_protector_active);
        if !protected_raw_parent && !protected_parent_family {
            if let Some(parent_node) = tree.nodes.get_mut(&tmeta.parent) {
                tb_disable_node(parent_node);
            }
        }
    }
}

fn tb_lite_on_tag_created(tag: u64, tmeta: &TagMeta) {
    if !rz_tb_lite_enabled() {
        return;
    }

    let kind = match tmeta.kind {
        PtrKind::RefShared => BorrowKind::Shared,
        PtrKind::RefMut => BorrowKind::Unique,
        PtrKind::RawConst => BorrowKind::RawConst,
        PtrKind::RawMut => BorrowKind::RawMut,
        _ => return,
    };

    let base = tb_base_for_addr(tmeta.pointee_addr);
    let mut all = tb_state().lock().unwrap();
    let tree = all.entry(base).or_default();
    if tb_lite_should_defer_raw_family(tmeta, kind) {
        return;
    }
    let node = tb_lite_insert_tag_node(tree, tag, tmeta, kind);
    let parent = node.parent;
    let protected = node.protected;
    let returned_carrier_reroot = (tmeta.lineage_hint & TB_LITE_HINT_RETURNED_CARRIER_REROOT) != 0;
    // Same-slot returned-carrier rerooting is write-like. A shared root is only a read view; it
    // must not retire an older unique family such as a two-phase receiver reservation.
    if returned_carrier_reroot && matches!(kind, BorrowKind::Unique) {
        let stack_like_root_ref = parent == 0 && {
            let amap = allocs().lock().unwrap();
            find_alloc_containing(&amap, tmeta.pointee_addr)
                .map(|(_base, meta)| meta.is_stack)
                .unwrap_or(false)
        };
        if stack_like_root_ref {
            let superseded_roots: Vec<u64> = tree
                .nodes
                .values()
                .filter(|n| n.tag != tag)
                .filter(|n| n.parent == 0)
                .filter(|n| tb_is_live_node(n))
                .filter(|n| matches!(n.kind, BorrowKind::Shared | BorrowKind::Unique))
                .filter(|n| !n.protected)
                .filter(|n| n.start == node.start && n.len == node.len)
                .map(|n| n.tag)
                .collect();
            if !superseded_roots.is_empty() {
                let superseded_tags: Vec<u64> = tree
                    .nodes
                    .values()
                    .filter(|n| {
                        superseded_roots
                            .iter()
                            .any(|root| n.tag == *root || tb_is_ancestor(&tree.nodes, *root, n.tag))
                    })
                    .map(|n| n.tag)
                    .collect();
                for superseded in superseded_tags {
                    if let Some(old) = tree.nodes.get_mut(&superseded) {
                        tb_disable_node(old);
                    }
                }
            }
        }
    }
    if kind == BorrowKind::Unique && protected {
        tb_shadow_same_slot_protected_unique_ancestors(tree, tag, node.start, node.len);
    }

    // Tree Borrows treats `&mut` creation as a reserved borrow. It does not by itself perform the
    // write-like invalidation that Stacked Borrows would perform; conflicts are decided when the
    // new borrow is actually read/written. Keeping creation side-effect-free is required for
    // valid TB patterns with multiple reserved `&mut` values and with `copy_nonoverlapping`
    // arguments built in either order.
    //
    // Function-entry protectors are stronger: creating a protected unique borrow must rule out
    // pre-existing sibling aliases that could be used during the protected call.
    if kind == BorrowKind::Unique && protected {
        let victim_tags: Vec<u64> = tree
            .nodes
            .values()
            .filter(|n| tb_is_live_node(n) && n.tag != tag)
            .filter(|n| matches!(n.kind, BorrowKind::RawConst | BorrowKind::RawMut))
            .filter(|n| tb_ranges_overlap(node.start, node.len, n.start, n.len))
            .filter(|n| !tb_is_ancestor(&tree.nodes, n.tag, tag))
            .filter(|n| !tb_is_ancestor(&tree.nodes, tag, n.tag))
            .map(|n| n.tag)
            .collect();
        for victim in victim_tags {
            let read_epoch = tree.foreign_read_epoch;
            if let Some(n) = tree.nodes.get_mut(&victim) {
                n.perm = TbPerm::Frozen;
                tb_set_lazy_perm(n, TbPerm::Frozen, read_epoch);
                n.alive = true;
                n.poisoned_by_protector_end = true;
            }
        }
    }
}

// Protector approximation used by tb_lite:
// - when an argument retag consumes a caller parent tag, we store that parent tag in the
//   current call frame (`pending_parent_tags`);
// - the immediate child Ref created from that parent becomes "protected" for the duration
//   of the frame (until `__rz_exit_fn`).
//
// Example:
//   fn callee(x: &mut u8, y: *mut u8) { unsafe { *y = 1; } }
//   let n = &mut 0u8;
//   let y = n as *mut u8;
//   callee(n, y); // x is protected in callee; write through y should violate.
/// Convert a pending call-arg parent into an active protected child.
///
/// For nested same-slot `&mut` calls we keep only the innermost protected Unique active; older
/// protected Unique ancestors remain live but are shadowed until the inner call exits.
fn tb_lite_mark_protected_if_pending(tag: u64, parent: u64, kind: BorrowKind) -> bool {
    if !matches!(kind, BorrowKind::Shared | BorrowKind::Unique) || parent == 0 {
        return false;
    }
    let mut frames = tb_protector_frames().lock().unwrap();
    let thread_id = std::thread::current().id();
    let Some(top) = frames.iter_mut().rfind(|f| f.thread_id == thread_id) else {
        return false;
    };
    let Some(pos) = top.pending_parent_tags.iter().position(|p| *p == parent) else {
        return false;
    };
    top.pending_parent_tags.swap_remove(pos);
    top.protected_tags.push(tag);
    true
}

fn tb_lite_mark_inplace_protected_if_pending(
    tag: u64,
    parent: u64,
    raw_parent: u64,
    addr: usize,
    kind: BorrowKind,
) {
    if !matches!(kind, BorrowKind::RawConst | BorrowKind::RawMut) {
        return;
    }
    let mut frames = tb_protector_frames().lock().unwrap();
    let thread_id = std::thread::current().id();
    let Some(top) = frames.iter_mut().rfind(|f| f.thread_id == thread_id) else {
        return;
    };
    let Some(pos) =
        top.pending_inplace_parent_tags
            .iter()
            .position(|(pending_parent, pending_addr)| {
                (*pending_parent == parent || *pending_parent == raw_parent)
                    && *pending_addr == addr
            })
    else {
        return;
    };
    top.pending_inplace_parent_tags.swap_remove(pos);
    top.inplace_protected_tags.push(tag);
}

fn tb_lite_inplace_protected_tag(tag: u64) -> bool {
    if tag == 0 {
        return false;
    }
    let frames = tb_protector_frames().lock().unwrap();
    let thread_id = std::thread::current().id();
    frames
        .iter()
        .rev()
        .any(|f| f.thread_id == thread_id && f.inplace_protected_tags.iter().any(|t| *t == tag))
}

/// Nested `&mut self` helpers on the same exact slot should transfer protector ownership to the
/// innermost protected Unique child instead of keeping every ancestor protector simultaneously
/// active. Shadowed ancestors stay live in the lineage but do not participate in protector or
/// 2-phase diagnostics until the child call exits.
fn tb_shadow_same_slot_protected_unique_ancestors(
    tree: &mut TbAllocState,
    child_tag: u64,
    start: usize,
    len: usize,
) {
    let mut cur = tree
        .nodes
        .get(&child_tag)
        .map(|node| node.parent)
        .unwrap_or(0);
    while cur != 0 {
        let next = tree.nodes.get(&cur).map(|node| node.parent).unwrap_or(0);
        if let Some(node) = tree.nodes.get_mut(&cur) {
            if matches!(node.kind, BorrowKind::Unique)
                && node.protected
                && node.start == start
                && node.len == len
            {
                node.protector_shadow_depth = node.protector_shadow_depth.saturating_add(1);
            }
        }
        cur = next;
    }
}

/// Restore same-slot protected Unique ancestors previously shadowed by a nested protected child.
fn tb_unshadow_same_slot_protected_unique_ancestors(
    tree: &mut TbAllocState,
    child_tag: u64,
    start: usize,
    len: usize,
) {
    let mut cur = tree
        .nodes
        .get(&child_tag)
        .map(|node| node.parent)
        .unwrap_or(0);
    while cur != 0 {
        let next = tree.nodes.get(&cur).map(|node| node.parent).unwrap_or(0);
        if let Some(node) = tree.nodes.get_mut(&cur) {
            if matches!(node.kind, BorrowKind::Unique)
                && node.protected
                && node.start == start
                && node.len == len
                && node.protector_shadow_depth != 0
            {
                node.protector_shadow_depth -= 1;
            }
        }
        cur = next;
    }
}

/// Re-enable an ordinary return family that is exported after call-exit teardown has already run.
///
/// Normal returned refs must not get broad ancestor revival before the return-boundary validation:
/// Tree Borrows intentionally rejects a returned `&mut` that was frozen/invalidated before return.
/// This helper therefore keeps the green-base behavior:
/// - always re-enable the exported exact tag itself
/// - additionally revive protector-end-disabled reserved ancestors only when the exported ref is
///   still a reserved borrow
///
/// That keeps caller-visible returned lineages attached to a live family without reviving
/// unrelated poisoned raw ancestors or masking ordinary returned-reference UB.
fn tb_lite_on_return_export(tag: u64, addr: usize) {
    if !rz_tb_lite_enabled() || tag == 0 {
        return;
    }

    let Some(tmeta) = tags().lock().unwrap().get(&tag).copied() else {
        return;
    };
    let addr = if addr != 0 { addr } else { tmeta.pointee_addr };
    if addr == 0 {
        return;
    }

    let base = tb_base_for_addr(tmeta.pointee_addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return;
    };
    tb_sync_node_lazy_reads(tree, tag);
    let read_epoch = tree.foreign_read_epoch;
    let Some(node) = tree.nodes.get_mut(&tag) else {
        return;
    };
    if node.start != addr {
        return;
    }
    if node.poisoned_by_protector_end
        && matches!(node.kind, BorrowKind::RawConst | BorrowKind::RawMut)
    {
        return;
    }
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        tb_reenable_exported_exact_node(node, read_epoch);
        return;
    }
    let revive_reserved_ancestors = matches!(node.perm, TbPerm::Reserved { .. });
    tb_reenable_exported_exact_node(node, read_epoch);
    if !revive_reserved_ancestors {
        return;
    }

    let mut cur = tree.nodes.get(&tag).map(|node| node.parent).unwrap_or(0);
    while cur != 0 {
        tb_sync_node_lazy_reads(tree, cur);
        let read_epoch = tree.foreign_read_epoch;
        let next = tree.nodes.get(&cur).map(|node| node.parent).unwrap_or(0);
        if let Some(node) = tree.nodes.get_mut(&cur) {
            if node.poisoned_by_protector_end && matches!(node.lazy_perm, TbPerm::Reserved { .. }) {
                tb_revive_node_after_protector_end(node, read_epoch);
            }
        }
        cur = next;
    }
}

/// Repair a caller-owned mutable argument family exported back after a call.
///
/// Mut-arg-ret export is not an ordinary returned reference. It is the callee writing back the
/// surviving lineage for a caller-owned `&mut T` carrier. If protector teardown disabled an older
/// same-lineage Unique ancestor before the survivor was exported, that ancestor is only an
/// obsolete local handle. Mark it `ShadowedLocal` instead of leaving a hard-dead ancestor that
/// later breaks the caller's live child lineage.
fn tb_lite_on_mut_arg_ret_export(tag: u64, addr: usize) {
    if !rz_tb_lite_enabled() || tag == 0 || addr == 0 {
        return;
    }

    let Some(tmeta) = tags().lock().unwrap().get(&tag).copied() else {
        return;
    };
    let base = tb_base_for_addr(tmeta.pointee_addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return;
    };
    tb_sync_node_lazy_reads(tree, tag);
    let read_epoch = tree.foreign_read_epoch;
    let Some(node) = tree.nodes.get_mut(&tag) else {
        return;
    };
    if node.start != addr {
        return;
    }
    if node.poisoned_by_protector_end
        && matches!(node.kind, BorrowKind::RawConst | BorrowKind::RawMut)
    {
        return;
    }
    let exported_start = node.start;
    let exported_len = node.len;
    tb_reenable_exported_exact_node(node, read_epoch);

    let mut cur = tree.nodes.get(&tag).map(|node| node.parent).unwrap_or(0);
    while cur != 0 {
        tb_sync_node_lazy_reads(tree, cur);
        let read_epoch = tree.foreign_read_epoch;
        let next = tree.nodes.get(&cur).map(|node| node.parent).unwrap_or(0);
        if let Some(node) = tree.nodes.get_mut(&cur) {
            if tb_range_covers(node.start, node.len, exported_start, exported_len) {
                if matches!(node.kind, BorrowKind::Unique)
                    && (matches!(node.perm, TbPerm::Disabled)
                        || matches!(node.lazy_perm, TbPerm::Disabled)
                        || (node.poisoned_by_protector_end
                            && matches!(node.lazy_perm, TbPerm::Reserved { .. })))
                {
                    tb_shadow_local_node(node, read_epoch);
                } else if node.poisoned_by_protector_end
                    && matches!(node.lazy_perm, TbPerm::Reserved { .. })
                {
                    tb_revive_node_after_protector_end(node, read_epoch);
                }
            }
        }
        cur = next;
    }
}

/// Retire a tag whose MIR local died or was overwritten.
///
/// This keeps temporary refs/raws from lingering as live TB siblings after the source local no
/// longer exists. Descendants remain governed by their own liveness; only the killed local's
/// exact node is retired here.
fn tb_lite_on_tag_killed(tag: u64) {
    if !rz_tb_lite_enabled() || tag == 0 {
        return;
    }

    let compact_invalidated = rz_tb_compact_invalidated_enabled();
    let Some(tmeta) = tags().lock().unwrap().get(&tag).copied() else {
        return;
    };
    let base = tb_base_for_addr(tmeta.pointee_addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return;
    };
    let has_live_descendant = tb_has_live_descendant(tree, tag);
    let read_epoch = tree.foreign_read_epoch;
    let Some(node) = tree.nodes.get_mut(&tag) else {
        return;
    };
    let parent = node.parent;
    let killed_readonly = matches!(node.kind, BorrowKind::Shared | BorrowKind::RawConst);
    if has_live_descendant && matches!(node.kind, BorrowKind::Unique) {
        tb_shadow_local_node(node, read_epoch);
    } else {
        tb_disable_node(node);
    }
    if killed_readonly {
        tb_reactivate_frozen_unique_ancestors_without_readers(tree, parent);
    }
    let _ = tb_compact_invalidated_roots(tree, &[tag], compact_invalidated);
}

fn tb_compacted_invalidated_hit(tags: &[u64]) -> Option<TbCompactedNode> {
    let compacted = tb_compacted_invalidated().lock().unwrap();
    tags.iter()
        .copied()
        .filter(|tag| *tag != 0)
        .find_map(|tag| compacted.get(&tag).copied())
}

fn tb_lite_invalidated_tombstone_msg(
    compacted: TbCompactedNode,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
    access: AliasAccessKind,
) -> String {
    format!(
        "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INVALIDATED compacted_kind={:?} compacted_epoch={} compacted_range=[0x{:x},0x{:x})",
        tb_access_name(access),
        compacted.tag,
        addr,
        size,
        tmeta.kind,
        compacted.kind,
        compacted.alloc_epoch,
        compacted.start,
        compacted.start.saturating_add(compacted.len)
    )
}

fn tb_protector_frame_tag_snapshot() -> Vec<u64> {
    let frames = tb_protector_frames().lock().unwrap();
    let mut tags = Vec::new();
    for frame in frames.iter() {
        tags.extend(frame.pending_parent_tags.iter().copied());
        tags.extend(frame.protected_tags.iter().copied());
        tags.extend(
            frame
                .pending_inplace_parent_tags
                .iter()
                .map(|(tag, _)| *tag),
        );
        tags.extend(frame.inplace_protected_tags.iter().copied());
    }
    tags
}

fn tb_subtree_tags_deepest_first(tree: &TbAllocState, root: u64) -> Vec<u64> {
    if !tree.nodes.contains_key(&root) {
        return Vec::new();
    }
    let mut tags = Vec::new();
    let mut stack = vec![(root, 0usize)];
    let mut visited = 0usize;
    while let Some((tag, depth)) = stack.pop() {
        if visited > tree.nodes.len() {
            break;
        }
        visited = visited.saturating_add(1);
        if !tree.nodes.contains_key(&tag) {
            continue;
        }
        tags.push((depth, tag));
        if let Some(children) = tree.children.get(&tag) {
            stack.extend(
                children
                    .iter()
                    .copied()
                    .map(|child| (child, depth.saturating_add(1))),
            );
        }
    }
    tags.sort_by(|(left_depth, left_tag), (right_depth, right_tag)| {
        right_depth
            .cmp(left_depth)
            .then_with(|| right_tag.cmp(left_tag))
    });
    tags.into_iter().map(|(_, tag)| tag).collect()
}

fn tb_descendant_tags(tree: &TbAllocState, root: u64) -> Vec<u64> {
    let mut tags = Vec::new();
    let mut stack = tree.children.get(&root).cloned().unwrap_or_default();
    let mut visited = 0usize;
    while let Some(tag) = stack.pop() {
        if visited > tree.nodes.len() {
            break;
        }
        visited = visited.saturating_add(1);
        if !tree.nodes.contains_key(&tag) {
            continue;
        }
        tags.push(tag);
        if let Some(children) = tree.children.get(&tag) {
            stack.extend(children.iter().copied());
        }
    }
    tags
}

fn tb_has_live_descendant(tree: &TbAllocState, root: u64) -> bool {
    let mut stack = tree.children.get(&root).cloned().unwrap_or_default();
    let mut visited = 0usize;
    while let Some(tag) = stack.pop() {
        if visited > tree.nodes.len() {
            break;
        }
        visited = visited.saturating_add(1);
        let Some(node) = tree.nodes.get(&tag) else {
            continue;
        };
        if tb_is_live_node(node) {
            return true;
        }
        if let Some(children) = tree.children.get(&tag) {
            stack.extend(children.iter().copied());
        }
    }
    false
}

fn tb_has_live_same_slot_unique_descendant(
    tree: &TbAllocState,
    root: u64,
    start: usize,
    len: usize,
) -> bool {
    let mut stack = tree.children.get(&root).cloned().unwrap_or_default();
    let mut visited = 0usize;
    while let Some(tag) = stack.pop() {
        if visited > tree.nodes.len() {
            break;
        }
        visited = visited.saturating_add(1);
        let Some(node) = tree.nodes.get(&tag) else {
            continue;
        };
        if tb_is_live_node(node)
            && matches!(node.kind, BorrowKind::Unique)
            && node.start == start
            && node.len == len
        {
            return true;
        }
        if let Some(children) = tree.children.get(&tag) {
            stack.extend(children.iter().copied());
        }
    }
    false
}

fn tb_returned_lineage_needs_tag(
    tree: &TbAllocState,
    tag: u64,
    returned_tags: &HashSet<u64>,
) -> bool {
    returned_tags
        .iter()
        .any(|returned| *returned == tag || tb_is_ancestor(&tree.nodes, tag, *returned))
}

fn tb_node_can_compact_invalidated(node: &TbNode, frame_tags: &[u64]) -> bool {
    if node.tag == 0 || tb_is_live_node(node) {
        return false;
    }
    // Raw and Unique tags can be valid call-boundary carriers before the return side channel
    // marks them as escaped/surviving. Until instrumentation exposes an explicit
    // "pending return survivor" bit, compact only dead shared helper views.
    if !matches!(node.kind, BorrowKind::Shared) {
        return false;
    }
    if matches!(node.perm, TbPerm::ShadowedLocal) || matches!(node.lazy_perm, TbPerm::ShadowedLocal)
    {
        return false;
    }
    if node.protected || node.protector_shadow_depth != 0 || node.poisoned_by_protector_end {
        return false;
    }
    if frame_tags.iter().any(|tag| *tag == node.tag) {
        return false;
    }
    // A local holder or escaped copy is not a reason to keep a disabled shared node in the active
    // tree. Future exact use of that tag must report stale provenance, and the tombstone preserves
    // enough metadata for that diagnostic without making every later access scan this node.
    matches!(
        tag_store::tag_lifecycle_state(node.tag),
        tag_store::TagLifecycleState::Active | tag_store::TagLifecycleState::HistoricalLive
    )
}

fn tb_compact_invalidated_roots(
    tree: &mut TbAllocState,
    roots: &[u64],
    compact_invalidated: bool,
) -> usize {
    if !compact_invalidated || roots.is_empty() {
        return 0;
    }

    let frame_tags = tb_protector_frame_tag_snapshot();
    let mut seen = Vec::new();
    let mut removed = 0usize;
    for root in roots.iter().copied().filter(|root| *root != 0) {
        if seen.iter().any(|seen_root| *seen_root == root) {
            continue;
        }
        seen.push(root);
        removed = removed.saturating_add(tb_compact_unreachable_invalidated_subtree(
            tree,
            root,
            &frame_tags,
        ));
    }
    removed
}

fn tb_compact_unreachable_invalidated_subtree(
    tree: &mut TbAllocState,
    root: u64,
    frame_tags: &[u64],
) -> usize {
    if root == 0 {
        return 0;
    }

    let subtree = tb_subtree_tags_deepest_first(tree, root);
    if subtree.is_empty() {
        return 0;
    }
    if subtree.iter().any(|tag| {
        tree.nodes.get(tag).map_or(true, |node| {
            !tb_node_can_compact_invalidated(node, frame_tags)
        })
    }) {
        return 0;
    }

    let mut compacted_nodes = Vec::new();
    for tag in &subtree {
        let Some(node) = tree.nodes.get(tag) else {
            return 0;
        };
        let base = tb_base_for_addr(node.start);
        compacted_nodes.push(TbCompactedNode {
            tag: node.tag,
            base,
            alloc_epoch: node.alloc_epoch,
            kind: node.kind,
            start: node.start,
            len: node.len,
        });
    }

    {
        let mut tombstones = tb_compacted_invalidated().lock().unwrap();
        for compacted in &compacted_nodes {
            tombstones.insert(compacted.tag, *compacted);
        }
    }

    let mut removed = 0usize;
    for compacted in compacted_nodes {
        if tag_store::compact_invalidated_tag(compacted.tag) {
            tag_pruning::note_invalidated_tag_compacted(compacted.tag);
            tree.remove_node(compacted.tag);
            removed = removed.saturating_add(1);
        } else {
            tb_compacted_invalidated()
                .lock()
                .unwrap()
                .remove(&compacted.tag);
        }
    }
    removed
}

fn tb_transition_candidate_node(
    tree: &TbAllocState,
    node: &TbNode,
    access_tag: u64,
    tmeta: &TagMeta,
    access: AliasAccessKind,
) -> bool {
    let relevant_state = tb_is_live_node(node)
        || (matches!(node.perm, TbPerm::Disabled)
            && matches!(node.kind, BorrowKind::Unique)
            && !(matches!(access, AliasAccessKind::Read)
                && matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RawConst))
            && tb_is_ancestor(&tree.nodes, node.tag, access_tag))
        || (matches!(node.perm, TbPerm::Disabled)
            && matches!(node.kind, BorrowKind::Shared)
            && matches!(access, AliasAccessKind::Write)
            && tb_is_ancestor(&tree.nodes, node.tag, access_tag));
    if !relevant_state {
        return false;
    }
    tmeta.alloc_epoch == 0 || node.alloc_epoch == 0 || node.alloc_epoch == tmeta.alloc_epoch
}

fn tb_push_unique_tag(tags: &mut Vec<u64>, tag: u64) {
    if tag != 0 && !tags.iter().any(|seen| *seen == tag) {
        tags.push(tag);
    }
}

fn tb_indexed_read_candidate_upper_bound(
    tree: &TbAllocState,
    access_lineage_len: usize,
    addr: usize,
    size: usize,
) -> usize {
    let mut count = access_lineage_len.saturating_add(tree.protected_tags.len());
    for key in tb_range_index_keys(addr, size)
        .into_iter()
        .chain(std::iter::once(TB_RANGE_INDEX_WIDE_BUCKET))
    {
        if let Some(indexed) = tree.range_index.get(&key) {
            count = count.saturating_add(indexed.len());
        }
    }
    count
}

fn tb_indexed_read_candidate_tags(
    tree: &TbAllocState,
    access_lineage: &[u64],
    access_tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> Option<Vec<u64>> {
    let node_count = tree.nodes.len();
    let indexed_upper_bound =
        tb_indexed_read_candidate_upper_bound(tree, access_lineage.len(), addr, size);
    if indexed_upper_bound.saturating_mul(TB_RANGE_INDEX_MIN_SCAN_REDUCTION) >= node_count {
        return None;
    }

    let mut tags = Vec::new();
    for tag in access_lineage {
        tb_push_unique_tag(&mut tags, *tag);
    }
    for tag in tree.overlapping_indexed_tags(addr, size) {
        tb_push_unique_tag(&mut tags, tag);
    }
    for tag in &tree.protected_tags {
        tb_push_unique_tag(&mut tags, *tag);
    }

    tags.retain(|tag| {
        tree.nodes.get(tag).is_some_and(|node| {
            tb_transition_candidate_node(tree, node, access_tag, tmeta, AliasAccessKind::Read)
        })
    });
    Some(tags)
}

fn tb_full_transition_candidate_tags(
    tree: &TbAllocState,
    access_tag: u64,
    tmeta: &TagMeta,
    access: AliasAccessKind,
) -> Vec<u64> {
    tree.nodes
        .values()
        .filter(|node| tb_transition_candidate_node(tree, node, access_tag, tmeta, access))
        .map(|node| node.tag)
        .collect()
}

fn tb_lite_check(
    sb_tag: u64,
    orig_tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
    access: AliasAccessKind,
) -> Option<String> {
    if !rz_tb_lite_enabled() || tmeta.alias_exempt || rz_sb_suppressed() {
        return None;
    }
    if !matches!(
        tmeta.kind,
        PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
    ) {
        return None;
    }

    if let Some(compacted) = tb_compacted_invalidated_hit(&[orig_tag, sb_tag]) {
        return Some(tb_lite_invalidated_tombstone_msg(
            compacted, tmeta, addr, size, access,
        ));
    }

    // The env-gate cache may allocate on first use. Initialize it before holding TB_STATE so
    // allocation bookkeeping cannot re-enter this alias model while the tree mutex is held.
    let compact_invalidated = rz_tb_compact_invalidated_enabled();

    let base = tb_base_for_addr(addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        if let Some(compacted) = tb_compacted_invalidated_hit(&[orig_tag, sb_tag]) {
            return Some(tb_lite_invalidated_tombstone_msg(
                compacted, tmeta, addr, size, access,
            ));
        }
        return None;
    };

    if matches!(access, AliasAccessKind::Write)
        && matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut)
        && (tmeta.lineage_hint & TB_LITE_HINT_RAW_REUSE_PARENT_FAMILY) != 0
        && !tree.nodes.contains_key(&orig_tag)
    {
        let _ = tb_lite_materialize_deferred_raw_node(tree, orig_tag, tmeta);
    }

    if matches!(access, AliasAccessKind::Write) {
        tb_lite_materialize_and_disable_deferred_raw_parent_of_ref_write(tree, tmeta);
    }

    // Use the original tag when TB tracked it (notably raw tags); otherwise fall back to
    // the nearest-ref tag used by the generic fast path.
    let mut access_tag = if tree.nodes.contains_key(&orig_tag) {
        orig_tag
    } else {
        sb_tag
    };

    let Some(mut node) = tree.nodes.get(&access_tag).cloned() else {
        if let Some(compacted) = tb_compacted_invalidated_hit(&[orig_tag, sb_tag, access_tag]) {
            return Some(tb_lite_invalidated_tombstone_msg(
                compacted, tmeta, addr, size, access,
            ));
        }
        // Best-effort: missing node means missing model metadata, not definite UB.
        return None;
    };
    let mut access_lineage = tb_collect_lineage(&tree.nodes, access_tag);
    let mut recovered_const_write_root: Option<u64> = None;
    if matches!(access, AliasAccessKind::Write) {
        if let Some(recovered_tag) = tb_lite_recover_root_raw_mut_sibling_for_const_write(
            tree,
            access_tag,
            &node,
            addr,
            size,
            tmeta.alloc_epoch,
        ) {
            recovered_const_write_root = Some(access_tag);
            access_tag = recovered_tag;
            if let Some(recovered_node) = tree.nodes.get(&access_tag).cloned() {
                node = recovered_node;
                access_lineage = tb_collect_lineage(&tree.nodes, access_tag);
            }
        }
    }
    if tmeta.alloc_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != tmeta.alloc_epoch {
        // Tag metadata and TB node disagree on epoch; treat as stale model state and skip.
        return None;
    }
    let dump = if rz_tb_dump_enabled() {
        tb_dump(tree, access_tag, addr, size, access)
    } else {
        String::new()
    };
    if tb_lite_inplace_protected_tag(access_tag) || tb_lite_inplace_protected_tag(orig_tag) {
        let mut msg = format!(
            "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INPLACE_CALL_ARG",
            tb_access_name(access),
            access_tag,
            addr,
            size,
            tmeta.kind
        );
        msg.push_str(&dump);
        return Some(msg);
    }
    if !tb_is_live_node(&node) {
        // Invalidated-reference accesses are always reported in TB-lite.
        let mut msg = format!(
            "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INVALIDATED",
            tb_access_name(access),
            access_tag,
            addr,
            size,
            tmeta.kind
        );
        msg.push_str(&dump);
        return Some(msg);
    }

    if rz_tb_trace_enabled() {
        eprintln!(
            "[tb-trace] access={:?} orig_tag={} access_tag={} addr=0x{:x} size={} kind={:?} lineage={:?}",
            access, orig_tag, access_tag, addr, size, tmeta.kind, access_lineage
        );
        for traced in tree.nodes.values().filter(|n| {
            tmeta.alloc_epoch == 0 || n.alloc_epoch == 0 || n.alloc_epoch == tmeta.alloc_epoch
        }) {
            eprintln!(
                "[tb-trace]   node tag={} parent={} kind={:?} perm={:?} lazy_perm={:?} alive={} protected={} shadowed={} range=[0x{:x},0x{:x}) extras={:?}",
                traced.tag,
                traced.parent,
                traced.kind,
                traced.perm,
                traced.lazy_perm,
                traced.alive,
                traced.protected,
                traced.protector_shadow_depth,
                traced.start,
                traced.start.saturating_add(traced.len),
                traced.extra_ranges
            );
        }
    }

    if matches!(access, AliasAccessKind::Write)
        && matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut)
        && (tmeta.lineage_hint & TB_LITE_HINT_RAW_REUSE_PARENT_FAMILY) != 0
        && !tree.nodes.contains_key(&orig_tag)
    {
        let _ = tb_lite_materialize_deferred_raw_node(tree, orig_tag, tmeta);
        if access_tag != orig_tag && tree.nodes.contains_key(&orig_tag) {
            access_tag = orig_tag;
            if let Some(materialized_node) = tree.nodes.get(&access_tag).cloned() {
                node = materialized_node;
                access_lineage = tb_collect_lineage(&tree.nodes, access_tag);
            }
        }
    }

    if matches!(access, AliasAccessKind::Write) {
        tb_lite_prepare_local_write(
            tree,
            access_tag,
            &access_lineage,
            addr,
            size,
            tmeta.alloc_epoch,
            compact_invalidated,
        );
        if let Some(prepared_node) = tree.nodes.get(&access_tag).cloned() {
            node = prepared_node;
        }
    }

    // Apply a TB-lite transition to the nodes that can observe this access.
    //
    // Reads use the range/protector/lineage index. Nodes outside that candidate set are only
    // affected through their uncovered-range default permission, so they absorb the foreign-read
    // epoch lazily when they are touched later. Writes keep the conservative full scan because
    // write lazy semantics include raw-sibling exceptions tied to the current access node.
    let candidate_tags = if matches!(access, AliasAccessKind::Read) {
        tb_indexed_read_candidate_tags(tree, &access_lineage, access_tag, tmeta, addr, size)
            .unwrap_or_else(|| tb_full_transition_candidate_tags(tree, access_tag, tmeta, access))
    } else {
        tb_full_transition_candidate_tags(tree, access_tag, tmeta, access)
    };

    let mut updates: Vec<(u64, TbPerm, bool)> = Vec::new();
    let mut newly_accessed_ranges: Vec<u64> = Vec::new();
    for tag in candidate_tags.iter().copied() {
        tb_sync_node_lazy_reads(tree, tag);
        let Some(n) = tree.nodes.get(&tag).cloned() else {
            continue;
        };
        let child = tb_lineage_contains(&access_lineage, n.tag);
        let covered = tb_node_overlaps(&n, addr, size);
        // `perm` is for ranges this node already covers. `lazy_perm` is the
        // node's default permission for same-allocation bytes that have not
        // been materialized into this node's range yet.
        let old_perm = if covered { n.perm } else { n.lazy_perm };

        let next = match (access, child, old_perm, tb_protector_active(&n)) {
            // Child/local read: everything except Disabled is unchanged.
            (AliasAccessKind::Read, true, TbPerm::Disabled, _) => {
                let mut msg = format!(
                    "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_DISABLED_ANCESTOR ancestor_tag={}",
                    access_tag, addr, size, tmeta.kind, n.tag
                );
                msg.push_str(&dump);
                return Some(msg);
            }
            (AliasAccessKind::Read, true, perm, _) => perm,

            // Foreign read:
            // - protected Reserved becomes conflicted
            // - Active becomes Frozen (or Disabled if protected)
            // - Frozen/Disabled unchanged
            (AliasAccessKind::Read, false, TbPerm::Reserved { conflicted: false }, true) => {
                TbPerm::Reserved { conflicted: true }
            }
            (AliasAccessKind::Read, false, TbPerm::Reserved { .. }, _) => old_perm,
            (AliasAccessKind::Read, false, TbPerm::Active, true) => TbPerm::Disabled,
            (AliasAccessKind::Read, false, TbPerm::Active, false) => TbPerm::Frozen,
            (AliasAccessKind::Read, false, TbPerm::ShadowedLocal, true) => TbPerm::Disabled,
            (AliasAccessKind::Read, false, TbPerm::ShadowedLocal, false) => TbPerm::Frozen,
            (AliasAccessKind::Read, false, TbPerm::Frozen, _) => TbPerm::Frozen,
            (AliasAccessKind::Read, false, TbPerm::Disabled, _) => TbPerm::Disabled,

            // Child/local write:
            // - Reserved(conflicted) while protected is UB (2-phase noalias violation)
            // - Reserved/Active activate to Active
            // - Frozen/Disabled cannot be written through
            (AliasAccessKind::Write, true, TbPerm::Reserved { conflicted: true }, true) => {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_2PHASE_CONFLICT tag={}",
                    access_tag, addr, size, tmeta.kind, n.tag
                );
                msg.push_str(&dump);
                return Some(msg);
            }
            (AliasAccessKind::Write, true, TbPerm::Reserved { .. }, _) => TbPerm::Active,
            (AliasAccessKind::Write, true, TbPerm::Active, _) => TbPerm::Active,
            (AliasAccessKind::Write, true, TbPerm::ShadowedLocal, _) => TbPerm::ShadowedLocal,
            (AliasAccessKind::Write, true, TbPerm::Frozen, _)
                if n.tag == access_tag && matches!(tmeta.kind, PtrKind::RefMut) =>
            {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_FROZEN_WRITE",
                    access_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }
            (AliasAccessKind::Write, true, TbPerm::Frozen, _)
                if n.tag == access_tag
                    && matches!(tmeta.kind, PtrKind::RawMut)
                    && n.poisoned_by_protector_end =>
            {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_FROZEN_WRITE",
                    access_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }
            (AliasAccessKind::Write, true, TbPerm::Frozen, _) => {
                let rawmut_interior_mut_extent =
                    tb_raw_write_within_explicit_interior_mut_extent(tmeta, addr, size);
                if rawmut_interior_mut_extent {
                    n.perm
                } else {
                    let mut msg = format!(
                        "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_FROZEN_WRITE",
                        access_tag, addr, size, tmeta.kind
                    );
                    msg.push_str(&dump);
                    return Some(msg);
                }
            }
            (AliasAccessKind::Write, true, TbPerm::Disabled, _) => {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_DISABLED_WRITE",
                    access_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }

            // Foreign write: preserve the original raw-const family when we reinterpret
            // an administrative write through a same-place raw-mutable root sibling.
            (AliasAccessKind::Write, false, perm, _)
                if recovered_const_write_root.is_some_and(|raw_const_root| {
                    n.kind == BorrowKind::RawConst
                        && (n.tag == raw_const_root
                            || tb_is_ancestor(&tree.nodes, raw_const_root, n.tag))
                }) =>
            {
                perm
            }

            // Foreign write: same-parent raw-mutable siblings over the same exact place stay
            // writable.
            (AliasAccessKind::Write, false, perm, _)
                if node.kind == BorrowKind::RawMut
                    && n.kind == BorrowKind::RawMut
                    && node.parent == n.parent
                    && node.parent != 0
                    && n.start == node.start
                    && n.len == node.len =>
            {
                perm
            }

            // Foreign write: raw root siblings over the same exact place stay writable.
            // This covers allocator/admin patterns such as same-base `realloc` where multiple
            // raw-mutable roots can legitimately refer to the same allocation bytes.
            (AliasAccessKind::Write, false, perm, _)
                if node.kind == BorrowKind::RawMut
                    && node.parent == 0
                    && n.kind == BorrowKind::RawMut
                    && n.parent == 0
                    && n.start == node.start
                    && n.len == node.len
                    && !tb_has_live_non_raw_overlap(tree, addr, size, tmeta.alloc_epoch) =>
            {
                perm
            }

            // Foreign write: disable.
            (AliasAccessKind::Write, false, _, _) => TbPerm::Disabled,
        };

        if child && !covered {
            newly_accessed_ranges.push(tag);
        }
        if covered {
            if next != n.perm {
                updates.push((tag, next, true));
            }
        } else if next != n.lazy_perm {
            updates.push((tag, next, false));
        }
    }

    // Protector diagnostic: a foreign access that drives a protected node to Disabled
    // is immediate UB ("protected tags must never be Disabled").
    //
    // Includes both write AND read access paths (foreign read on a protected Active
    // Unique transitions to Disabled per the transition table; foreign write on any
    // non-exempt protected node lands at Disabled). Includes Shared-kind protected
    // (for `&T` args like in `invalidate_against_protector2` TB rev).
    //
    // We deliberately do NOT fire on transitions to Reserved{true} (conflicted) here:
    // a conflicted-Reserved may still be rescued if the surrounding context (interior
    // mutability via UnsafeCell) relaxes the transition back to Frozen at access time.
    // Reporting at access time would FP on legal `&mut UnsafeCell<T>` arg patterns
    // (see `miri_tb_pass_exact::reserved`, `interior_mutability`). Conflicted-at-
    // protector-release UB is covered by the poisoned_by_protector_end path.
    {
        // For Shared-kind protected nodes, exclude those whose pointee carries interior
        // mutability (UnsafeCell / RefCell / Cell etc.). Instrumentation sets
        // `alias_exempt` on the tag in that case. `&UnsafeCell<_>` foreign accesses
        // through the interior-mut path are deliberately not UB under TB.
        let tmap_for_exempt = tags().lock().unwrap();
        let mut protected_nodes: Vec<TbNode> = tree
            .protected_tags
            .iter()
            .filter_map(|tag| tree.nodes.get(tag))
            .filter(|n| {
                tb_is_live_node(n)
                    && tb_protector_active(n)
                    && n.tag != access_tag
                    && (tmeta.alloc_epoch == 0
                        || n.alloc_epoch == 0
                        || n.alloc_epoch == tmeta.alloc_epoch)
                    && match n.kind {
                        BorrowKind::Unique => true,
                        BorrowKind::Shared => !tmap_for_exempt
                            .get(&n.tag)
                            .map_or(false, |m| m.alias_exempt),
                        _ => false,
                    }
            })
            .cloned()
            .collect();
        drop(tmap_for_exempt);
        protected_nodes.sort_by_key(|n| n.tag);
        if !protected_nodes.is_empty() {
            let tmap = tags().lock().unwrap();
            for protected in protected_nodes {
                let touched_protected = updates.iter().any(|(t, next, covered)| {
                    *covered && *t == protected.tag && *next == TbPerm::Disabled
                });
                if !touched_protected {
                    continue;
                }
                if !tb_same_lineage_protected_conflict_ok(
                    &tree.nodes,
                    &tmap,
                    protected.tag,
                    access_tag,
                    addr,
                    size,
                ) {
                    let mut msg = format!(
                        "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_PROTECTOR_CONFLICT protected_tag={} protected_kind={:?}",
                        tb_access_name(access),
                        access_tag,
                        addr,
                        size,
                        tmeta.kind,
                        protected.tag,
                        protected.kind
                    );
                    msg.push_str(&dump);
                    return Some(msg);
                }
            }
        }
    }

    let next_read_epoch = if matches!(access, AliasAccessKind::Read) {
        let next = tree.foreign_read_epoch.saturating_add(1);
        tree.foreign_read_epoch = next;
        Some(next)
    } else {
        None
    };

    let access_len = tb_effective_access_len(size);
    let mut compact_roots = Vec::new();
    let current_read_epoch = tree.foreign_read_epoch;
    for (tag, next, covered) in updates {
        if let Some(n) = tree.nodes.get_mut(&tag) {
            if covered {
                n.perm = next;
                n.alive = next != TbPerm::Disabled;
                if next != TbPerm::Disabled {
                    n.poisoned_by_protector_end = false;
                } else {
                    compact_roots.push(tag);
                }
            } else {
                tb_set_lazy_perm(n, next, current_read_epoch);
            }
        }
    }
    if let Some(epoch) = next_read_epoch {
        for tag in &candidate_tags {
            if let Some(node) = tree.nodes.get_mut(tag) {
                node.lazy_read_epoch = epoch;
            }
        }
    }
    let mut extra_ranges_to_index = Vec::new();
    for tag in newly_accessed_ranges {
        if let Some(n) = tree.nodes.get_mut(&tag) {
            if !tb_ranges_overlap(addr, access_len, n.start, n.len)
                && !n
                    .extra_ranges
                    .iter()
                    .any(|(start, len)| tb_ranges_overlap(addr, access_len, *start, *len))
            {
                n.extra_ranges.push((addr, access_len));
                n.perm = n.lazy_perm;
                n.alive = n.perm != TbPerm::Disabled;
                extra_ranges_to_index.push((tag, addr, access_len));
                if !n.alive {
                    compact_roots.push(tag);
                }
            }
        }
    }
    for (tag, start, len) in extra_ranges_to_index {
        tree.index_range(tag, start, len);
    }
    let _ = tb_compact_invalidated_roots(tree, &compact_roots, compact_invalidated);

    None
}

fn tb_lite_recover_root_raw_mut_sibling_for_const_write(
    tree: &TbAllocState,
    raw_const_tag: u64,
    raw_const_node: &TbNode,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> Option<u64> {
    let access_len = tb_effective_access_len(size);
    if raw_const_node.kind != BorrowKind::RawConst
        || raw_const_node.parent != 0
        || raw_const_node.start != addr
        || raw_const_node.len >= access_len
        || tb_has_live_non_raw_overlap(tree, addr, size, alloc_epoch)
    {
        return None;
    }

    tree.nodes
        .values()
        .filter(|n| n.tag != raw_const_tag)
        .filter(|n| tb_is_live_node(n))
        .filter(|n| n.kind == BorrowKind::RawMut)
        .filter(|n| n.parent == 0)
        .filter(|n| n.start == raw_const_node.start && n.len == raw_const_node.len)
        .filter(|n| alloc_epoch == 0 || n.alloc_epoch == 0 || n.alloc_epoch == alloc_epoch)
        .map(|n| n.tag)
        .max()
}

fn tb_has_live_non_raw_overlap(
    tree: &TbAllocState,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> bool {
    tree.nodes.values().any(|n| {
        tb_is_live_node(n)
            && matches!(n.kind, BorrowKind::Shared | BorrowKind::Unique)
            && (alloc_epoch == 0 || n.alloc_epoch == 0 || n.alloc_epoch == alloc_epoch)
            && tb_node_overlaps(n, addr, size)
    })
}

#[inline]
fn tb_access_name(access: AliasAccessKind) -> &'static str {
    match access {
        AliasAccessKind::Read => "READ",
        AliasAccessKind::Write => "WRITE",
    }
}

fn tb_dump(
    tree: &TbAllocState,
    sb_tag: u64,
    addr: usize,
    size: usize,
    access: AliasAccessKind,
) -> String {
    let mut out = String::new();
    out.push_str("\n-- tb-lite dump --\n");
    out.push_str(&format!(
        "access={:?} tag={} addr=0x{:x} size={}\n",
        access, sb_tag, addr, size
    ));
    out.push_str("nodes:\n");
    let mut nodes: Vec<&TbNode> = tree.nodes.values().collect();
    nodes.sort_by_key(|n| n.tag);
    for n in nodes {
        out.push_str(&format!(
            "  tag={} parent={} epoch={} kind={:?} perm={:?} lazy_perm={:?} alive={} protected={} shadowed={} range=[0x{:x},0x{:x})\n",
            n.tag,
            n.parent,
            n.alloc_epoch,
            n.kind,
            n.perm,
            n.lazy_perm,
            n.alive,
            n.protected,
            n.protector_shadow_depth,
            n.start,
            n.start.saturating_add(n.len)
        ));
    }
    out.push_str("-- end tb-lite dump --\n");
    out
}

#[inline]
fn tb_node_len_for_meta(tmeta: &TagMeta) -> usize {
    // Keep precise empty slice/str views in the lineage without inflating them into synthetic
    // 1-byte authority. Unknown bounds still use the minimal 1-byte TB-lite footprint.
    if bounds_len_is_precise_empty(tmeta.bounds_len) {
        0
    } else {
        tb_effective_access_len(bounds_len_bytes_or_zero(tmeta.bounds_len))
    }
}

#[inline]
fn tb_effective_access_len(size: usize) -> usize {
    if size == 0 {
        1
    } else {
        size
    }
}

#[inline]
fn tb_base_for_addr(addr: usize) -> usize {
    let amap = allocs().lock().unwrap();
    find_alloc_containing(&amap, addr)
        .map(|(base, _)| base)
        .unwrap_or(addr)
}

fn tb_range_index_keys(start: usize, len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }

    let first = start / TB_RANGE_INDEX_BUCKET_SIZE;
    let last = start.saturating_add(len.saturating_sub(1)) / TB_RANGE_INDEX_BUCKET_SIZE;
    let bucket_count = last.saturating_sub(first).saturating_add(1);
    if bucket_count > TB_RANGE_INDEX_MAX_BUCKETS_PER_RANGE {
        return vec![TB_RANGE_INDEX_WIDE_BUCKET];
    }

    (first..=last).collect()
}

#[inline]
fn tb_ranges_overlap(a_start: usize, a_len: usize, b_start: usize, b_len: usize) -> bool {
    if a_len == 0 || b_len == 0 {
        return false;
    }
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[inline]
fn tb_range_covers(cover_start: usize, cover_len: usize, addr: usize, size: usize) -> bool {
    if cover_len == 0 || size == 0 {
        return false;
    }
    let cover_end = cover_start.saturating_add(cover_len);
    let access_end = addr.saturating_add(size);
    cover_start <= addr && access_end <= cover_end
}

#[inline]
/// Allow a frozen `RawMut` write only when instrumentation/runtime attached an
/// explicit writable extent for surrounding interior-mutable bytes and the
/// write stays inside that extent.
///
/// This is the principled replacement for the old `rawmut_uncovered` fallback:
/// writes do not become okay merely because the blocking node's covered range
/// is narrower than the written bytes. They are okay only when we can point to
/// an explicit permission region carried in tag metadata.
fn tb_raw_write_within_explicit_interior_mut_extent(
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> bool {
    if !matches!(tmeta.kind, PtrKind::RawMut) {
        return false;
    }
    tb_range_covers(
        tmeta.interior_mut_extent_base,
        tmeta.interior_mut_extent_len,
        addr,
        size,
    )
}

#[inline]
fn tb_node_overlaps(node: &TbNode, addr: usize, size: usize) -> bool {
    tb_ranges_overlap(addr, size, node.start, node.len)
        || node
            .extra_ranges
            .iter()
            .any(|(start, len)| tb_ranges_overlap(addr, size, *start, *len))
}

#[inline]
fn tb_node_covers(node: &TbNode, addr: usize, size: usize) -> bool {
    tb_range_covers(node.start, node.len, addr, size)
        || node
            .extra_ranges
            .iter()
            .any(|(start, len)| tb_range_covers(*start, *len, addr, size))
}

#[inline]
fn tb_is_live_node(n: &TbNode) -> bool {
    n.alive && n.perm != TbPerm::Disabled
}

#[inline]
fn tb_set_lazy_perm(n: &mut TbNode, perm: TbPerm, read_epoch: u64) {
    n.lazy_perm = perm;
    n.lazy_read_epoch = read_epoch;
}

#[inline]
fn tb_sync_node_lazy_reads_to(n: &mut TbNode, read_epoch: u64) {
    if n.lazy_read_epoch == read_epoch {
        return;
    }
    n.lazy_perm = match n.lazy_perm {
        TbPerm::Active | TbPerm::ShadowedLocal => TbPerm::Frozen,
        perm => perm,
    };
    n.lazy_read_epoch = read_epoch;
}

#[inline]
fn tb_sync_node_lazy_reads(tree: &mut TbAllocState, tag: u64) {
    let read_epoch = tree.foreign_read_epoch;
    if let Some(node) = tree.nodes.get_mut(&tag) {
        tb_sync_node_lazy_reads_to(node, read_epoch);
    }
}

#[inline]
fn tb_shadow_local_node(n: &mut TbNode, read_epoch: u64) {
    n.perm = TbPerm::ShadowedLocal;
    tb_set_lazy_perm(n, TbPerm::ShadowedLocal, read_epoch);
    n.alive = true;
    n.poisoned_by_protector_end = false;
}

#[inline]
fn tb_disable_node(n: &mut TbNode) {
    n.perm = TbPerm::Disabled;
    n.alive = false;
    n.poisoned_by_protector_end = false;
}

#[inline]
fn tb_disable_node_for_protector_end(n: &mut TbNode) {
    n.perm = TbPerm::Disabled;
    n.alive = false;
    n.poisoned_by_protector_end = true;
}

fn tb_has_live_readonly_descendant(
    tree: &TbAllocState,
    ancestor_tag: u64,
    addr: usize,
    size: usize,
) -> bool {
    let mut stack = tree
        .children
        .get(&ancestor_tag)
        .cloned()
        .unwrap_or_default();
    let mut visited = 0usize;
    while let Some(tag) = stack.pop() {
        if visited > tree.nodes.len() {
            break;
        }
        visited = visited.saturating_add(1);
        let Some(node) = tree.nodes.get(&tag) else {
            continue;
        };
        if tb_is_live_node(node)
            && matches!(node.kind, BorrowKind::Shared | BorrowKind::RawConst)
            && tb_node_overlaps(node, addr, size)
        {
            return true;
        }
        if let Some(children) = tree.children.get(&tag) {
            stack.extend(children.iter().copied());
        }
    }
    false
}

/// Retire temporary helper views before a real local mutable write.
///
/// Example: `r: &mut T` is read through a short-lived helper `tmp: &T`, then the
/// program writes through `r` again. The helper read may leave `tmp` as a frozen
/// node. That helper should be invalidated by the `r` write; it should not block
/// the write itself. After retiring such helpers, restore any local unique
/// ancestors that were frozen only because those helpers existed. If no helper
/// was retired, do not restore anything, so real alternating read/write
/// violations still fail.
fn tb_lite_prepare_local_write(
    tree: &mut TbAllocState,
    access_tag: u64,
    access_lineage: &[u64],
    addr: usize,
    size: usize,
    alloc_epoch: u64,
    compact_invalidated: bool,
) {
    let Some(access_node) = tree.nodes.get(&access_tag) else {
        return;
    };
    if !matches!(access_node.kind, BorrowKind::Unique | BorrowKind::RawMut) {
        return;
    }
    let collapse_raw_lineage_helpers = matches!(access_node.kind, BorrowKind::Unique);

    let readonly_blockers: Vec<u64> = tree
        .nodes
        .values()
        .filter(|node| node.tag != access_tag)
        .filter(|node| !tb_lineage_contains(access_lineage, node.tag))
        .filter(|node| tb_is_live_node(node))
        .filter(|node| matches!(node.kind, BorrowKind::Shared | BorrowKind::RawConst))
        .filter(|node| !tb_protector_active(node))
        .filter(|node| alloc_epoch == 0 || node.alloc_epoch == 0 || node.alloc_epoch == alloc_epoch)
        .filter(|node| tb_node_overlaps(node, addr, size))
        .map(|node| node.tag)
        .collect();

    let raw_lineage_helpers: Vec<u64> = if collapse_raw_lineage_helpers {
        let tmap = tags().lock().unwrap();
        access_lineage
            .iter()
            .filter(|tag| **tag != access_tag)
            .filter_map(|tag| tree.nodes.get(tag))
            .filter(|node| tb_is_live_node(node))
            .filter(|node| matches!(node.kind, BorrowKind::RawConst | BorrowKind::RawMut))
            .filter(|node| !tb_protector_active(node))
            .filter(|node| {
                alloc_epoch == 0 || node.alloc_epoch == 0 || node.alloc_epoch == alloc_epoch
            })
            .filter(|node| tb_lite_raw_transport_family_marked(&tmap, node.tag, node.kind))
            .map(|node| node.tag)
            .collect()
    } else {
        Vec::new()
    };
    let retired_helper = !readonly_blockers.is_empty() || !raw_lineage_helpers.is_empty();

    for tag in &readonly_blockers {
        if let Some(node) = tree.nodes.get_mut(tag) {
            tb_disable_node(node);
        }
    }
    for tag in &raw_lineage_helpers {
        if let Some(node) = tree.nodes.get_mut(tag) {
            tb_disable_node(node);
        }
    }

    if retired_helper {
        tb_reactivate_frozen_unique_ancestors_without_readers_for_access(
            tree, access_tag, addr, size,
        );
        let mut compact_roots = readonly_blockers.clone();
        compact_roots.extend(raw_lineage_helpers.iter().copied());
        let _ = tb_compact_invalidated_roots(tree, &compact_roots, compact_invalidated);
    }
}

// Some raw helpers are emitted as RawRoot after their immediate source was a
// tag-preserving raw transport. Treat that whole raw metadata chain as
// administrative when a real RefMut descendant performs the write.
fn tb_lite_raw_transport_family_marked(
    tmap: &HashMap<u64, TagMeta>,
    tag: u64,
    kind: BorrowKind,
) -> bool {
    if !matches!(kind, BorrowKind::RawConst | BorrowKind::RawMut) {
        return false;
    }

    let mut cursor = tag;
    for _ in 0..tmap.len().saturating_add(1) {
        let Some(meta) = tmap.get(&cursor) else {
            return false;
        };
        let meta_kind = match meta.kind {
            PtrKind::RawConst => BorrowKind::RawConst,
            PtrKind::RawMut => BorrowKind::RawMut,
            _ => return false,
        };
        if tb_lite_should_defer_raw_family(meta, meta_kind) {
            return true;
        }
        if meta.parent == 0 {
            return false;
        }
        cursor = meta.parent;
    }

    false
}

fn tb_reactivate_frozen_unique_ancestors_without_readers(
    tree: &mut TbAllocState,
    start_parent: u64,
) {
    tb_reactivate_frozen_unique_ancestors_without_readers_inner(tree, start_parent, None);
}

fn tb_reactivate_frozen_unique_ancestors_without_readers_for_access(
    tree: &mut TbAllocState,
    start_parent: u64,
    addr: usize,
    size: usize,
) {
    tb_reactivate_frozen_unique_ancestors_without_readers_inner(
        tree,
        start_parent,
        Some((addr, size)),
    );
}

fn tb_reactivate_frozen_unique_ancestors_without_readers_inner(
    tree: &mut TbAllocState,
    start_parent: u64,
    access_range: Option<(usize, usize)>,
) {
    let mut cursor = start_parent;
    for _ in 0..tree.nodes.len().saturating_add(1) {
        tb_sync_node_lazy_reads(tree, cursor);
        let Some((next, start, len, should_reactivate, restored_perm)) =
            tree.nodes.get(&cursor).map(|node| {
                let restored_perm = match node.lazy_perm {
                    TbPerm::Reserved { .. } | TbPerm::Active | TbPerm::ShadowedLocal => {
                        Some(node.lazy_perm)
                    }
                    TbPerm::Frozen | TbPerm::Disabled => None,
                };
                (
                    node.parent,
                    node.start,
                    node.len,
                    node.alive
                        && matches!(node.kind, BorrowKind::Unique)
                        && matches!(node.perm, TbPerm::Frozen)
                        && !node.poisoned_by_protector_end
                        && restored_perm.is_some(),
                    restored_perm,
                )
            })
        else {
            break;
        };

        let (check_start, check_len) = access_range.unwrap_or((start, len));
        if should_reactivate
            && !tb_has_live_readonly_descendant(tree, cursor, check_start, check_len)
        {
            if let Some(node) = tree.nodes.get_mut(&cursor) {
                if let Some(restored_perm) = restored_perm {
                    node.perm = restored_perm;
                }
            }
        }

        if next == 0 {
            break;
        }
        cursor = next;
    }
}

/// Revive a node that was disabled specifically by protector release.
///
/// This is narrower than exported-tag re-enable. It only applies to nodes marked
/// `poisoned_by_protector_end`, clears the protector bookkeeping, and restores the best surviving
/// permission from `lazy_perm` when that still carries useful state. We use it for reserved
/// ancestors that must stay live because a returned descendant still depends on their lineage.
#[inline]
fn tb_revive_node_after_protector_end(n: &mut TbNode, read_epoch: u64) {
    if !n.poisoned_by_protector_end {
        return;
    }
    n.protected = false;
    n.protector_shadow_depth = 0;
    n.poisoned_by_protector_end = false;
    n.alive = true;
    if matches!(n.perm, TbPerm::Disabled) {
        n.perm = if matches!(n.lazy_perm, TbPerm::Disabled) {
            match n.kind {
                BorrowKind::Unique | BorrowKind::RawMut => TbPerm::Active,
                BorrowKind::Shared | BorrowKind::RawConst => TbPerm::Frozen,
            }
        } else {
            n.lazy_perm
        };
    }
    if matches!(n.lazy_perm, TbPerm::Disabled) {
        tb_set_lazy_perm(n, n.perm, read_epoch);
    }
}

/// Re-enable the exact tag that is being exported back to the caller.
///
/// Exported exact tags are caller-visible by definition, so once an export hook publishes them we
/// must drop any frame-local protector bookkeeping and give them a live permission again. Unlike
/// `tb_revive_node_after_protector_end`, this helper is intentionally broader: it re-enables the
/// exported exact node even when it was disabled for reasons other than protector-end poison.
#[inline]
fn tb_reenable_exported_exact_node(n: &mut TbNode, read_epoch: u64) {
    n.protected = false;
    n.protector_shadow_depth = 0;
    n.poisoned_by_protector_end = false;
    n.alive = true;
    if matches!(n.perm, TbPerm::Disabled) {
        n.perm = match n.kind {
            BorrowKind::Unique | BorrowKind::RawMut => TbPerm::Active,
            BorrowKind::Shared | BorrowKind::RawConst => TbPerm::Frozen,
        };
    }
    if matches!(n.lazy_perm, TbPerm::Disabled) {
        tb_set_lazy_perm(n, n.perm, read_epoch);
    }
}

fn tb_collect_lineage(nodes: &HashMap<u64, TbNode>, mut tag: u64) -> Vec<u64> {
    let mut lineage = Vec::new();
    for _ in 0..nodes.len().saturating_add(1) {
        let Some(node) = nodes.get(&tag) else {
            break;
        };
        lineage.push(tag);
        if node.parent == 0 {
            break;
        }
        tag = node.parent;
    }
    lineage
}

#[inline]
fn tb_lineage_contains(lineage: &[u64], tag: u64) -> bool {
    lineage.contains(&tag)
}

fn tb_lite_find_ref_ancestor_tag(tmap: &HashMap<u64, TagMeta>, mut tag: u64) -> Option<u64> {
    for _ in 0..tmap.len().saturating_add(1) {
        let t = tmap.get(&tag)?;
        if matches!(t.kind, PtrKind::RefShared | PtrKind::RefMut) {
            return Some(tag);
        }
        if t.parent == 0 {
            return None;
        }
        tag = t.parent;
    }
    None
}

fn tb_lite_find_materialized_ref_ancestor_tag(
    tmap: &HashMap<u64, TagMeta>,
    nodes: &HashMap<u64, TbNode>,
    mut tag: u64,
) -> Option<u64> {
    for _ in 0..tmap.len().saturating_add(1) {
        let t = tmap.get(&tag)?;
        if nodes.contains_key(&tag) && matches!(t.kind, PtrKind::RefShared | PtrKind::RefMut) {
            return Some(tag);
        }
        if t.parent == 0 {
            return None;
        }
        tag = t.parent;
    }
    None
}

fn tb_lite_find_materialized_authority_ancestor_tag(
    tmap: &HashMap<u64, TagMeta>,
    nodes: &HashMap<u64, TbNode>,
    mut tag: u64,
    child_start: usize,
    child_len: usize,
    skip_covering_transport: bool,
) -> Option<u64> {
    for _ in 0..tmap.len().saturating_add(1) {
        if nodes.contains_key(&tag)
            && !tb_lite_raw_transport_parent_skippable(
                tmap,
                nodes,
                tag,
                child_start,
                child_len,
                skip_covering_transport,
            )
        {
            return Some(tag);
        }
        let t = tmap.get(&tag)?;
        if t.parent == 0 {
            return None;
        }
        tag = t.parent;
    }
    None
}

fn tb_lite_can_recover_parent_tag(tag: u64) -> bool {
    if !rz_tb_lite_enabled() || tag == 0 {
        return tag != 0;
    }
    let all = tb_state().lock().unwrap();
    for tree in all.values() {
        if let Some(node) = tree.nodes.get(&tag) {
            return tb_is_live_node(node);
        }
    }
    true
}

/// Canonicalize a mut-arg-ret export back to the stable live family that should survive the call.
///
/// Mut-arg-ret writeback is different from normal return export: the caller is refreshing the tag
/// attached to an existing `&mut T` carrier, not importing a brand-new returned reference. The
/// raw exported tag can therefore be a transient helper child that was valid inside the callee but
/// should not survive as the caller's long-lived carrier anchor. We walk upward and choose the
/// nearest live `Unique` family at the same exact place, falling back to the nearest live ref if
/// no such `Unique` survives.
fn tb_lite_canonicalize_mut_arg_ret_tag(tag: u64, addr: usize) -> u64 {
    if !rz_tb_lite_enabled() || tag == 0 || addr == 0 {
        return tag;
    }

    let base = tb_base_for_addr(addr);
    let all = tb_state().lock().unwrap();
    let Some(tree) = all.get(&base) else {
        return tag;
    };

    let mut cursor = tag;
    let mut best_live_ref = 0u64;
    for _ in 0..tree.nodes.len().saturating_add(1) {
        let Some(node) = tree.nodes.get(&cursor) else {
            break;
        };
        if node.start == addr && tb_is_live_node(node) && matches!(node.kind, BorrowKind::Unique) {
            return cursor;
        }
        if best_live_ref == 0
            && node.start == addr
            && tb_is_live_node(node)
            && matches!(node.kind, BorrowKind::Shared | BorrowKind::Unique)
        {
            best_live_ref = cursor;
        }
        if node.parent == 0 {
            break;
        }
        cursor = node.parent;
    }

    if best_live_ref != 0 {
        best_live_ref
    } else {
        tag
    }
}

#[inline]
fn tb_is_ancestor(nodes: &HashMap<u64, TbNode>, ancestor: u64, mut tag: u64) -> bool {
    if ancestor == tag {
        return true;
    }
    for _ in 0..nodes.len().saturating_add(1) {
        let Some(n) = nodes.get(&tag) else {
            return false;
        };
        if n.parent == 0 {
            return false;
        }
        if n.parent == ancestor {
            return true;
        }
        tag = n.parent;
    }
    false
}

fn tb_is_effective_ancestor(
    nodes: &HashMap<u64, TbNode>,
    tmap: &HashMap<u64, TagMeta>,
    ancestor: u64,
    mut tag: u64,
) -> bool {
    if tb_is_ancestor(nodes, ancestor, tag) {
        return true;
    }
    if ancestor == tag {
        return true;
    }
    for _ in 0..tmap.len().saturating_add(1) {
        let Some(t) = tmap.get(&tag) else {
            return false;
        };
        if t.parent == 0 {
            return false;
        }
        if t.parent == ancestor {
            return true;
        }
        tag = t.parent;
    }
    false
}

fn tb_same_lineage_protected_conflict_ok(
    nodes: &HashMap<u64, TbNode>,
    tmap: &HashMap<u64, TagMeta>,
    protected_tag: u64,
    access_tag: u64,
    addr: usize,
    size: usize,
) -> bool {
    if !tb_is_effective_ancestor(nodes, tmap, protected_tag, access_tag) {
        let Some(access_node) = nodes.get(&access_tag) else {
            return false;
        };
        let Some(protected_node) = nodes.get(&protected_tag) else {
            return false;
        };
        if !matches!(access_node.kind, BorrowKind::Unique)
            || !matches!(protected_node.kind, BorrowKind::Shared)
            || !tb_is_effective_ancestor(nodes, tmap, access_tag, protected_tag)
        {
            return false;
        }
    }
    nodes.values().all(|other| {
        if !tb_is_live_node(other) || !tb_node_overlaps(other, addr, size) {
            return true;
        }
        if !tmap
            .get(&other.tag)
            .map(|meta| meta.escaped)
            .unwrap_or(true)
            && !tag_store::active_tag_has_local_holder(other.tag)
        {
            return true;
        }
        tb_is_effective_ancestor(nodes, tmap, other.tag, access_tag)
            || tb_is_effective_ancestor(nodes, tmap, access_tag, other.tag)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PtrKind, TagMeta};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_TAG: AtomicU64 = AtomicU64::new(9_000_000_000);

    fn next_test_tag() -> u64 {
        NEXT_TEST_TAG.fetch_add(1, Ordering::Relaxed)
    }

    fn test_addr(tag: u64) -> usize {
        0x7000_0000usize + ((tag as usize) * 0x100)
    }

    fn test_meta(addr: usize, kind: PtrKind, parent: u64) -> TagMeta {
        TagMeta {
            pointee_addr: addr,
            kind,
            parent,
            escaped: false,
            alloc_epoch: 1,
            alloc_live_at_creation: true,
            alias_exempt: false,
            lineage_hint: 0,
            exposed_provenance_root: false,
            bounds_len: 1,
            interior_mut_extent_base: 0,
            interior_mut_extent_len: 0,
            align_req: 1,
            origin_known: true,
            origin_base: addr,
            origin_end: addr.saturating_add(1),
        }
    }

    fn test_node(tag: u64, parent: u64, addr: usize, kind: BorrowKind, perm: TbPerm) -> TbNode {
        TbNode {
            tag,
            parent,
            alloc_epoch: 1,
            kind,
            perm,
            lazy_perm: perm,
            lazy_read_epoch: 0,
            start: addr,
            len: 1,
            extra_ranges: Vec::new(),
            alive: perm != TbPerm::Disabled,
            protected: false,
            protector_shadow_depth: 0,
            poisoned_by_protector_end: false,
        }
    }

    #[test]
    fn compacted_invalidated_tag_reports_exact_stale_hit() {
        std::env::set_var("RZ_TB_COMPACT_INVALIDATED_TAGS", "1");

        let tag = next_test_tag();
        let addr = test_addr(tag);
        let meta = test_meta(addr, PtrKind::RefShared, 0);
        tag_store::insert(tag, meta);

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(
            tag,
            0,
            addr,
            BorrowKind::Shared,
            TbPerm::Disabled,
        ));

        let removed = tb_compact_unreachable_invalidated_subtree(&mut tree, tag, &[]);
        assert_eq!(removed, 1);
        assert!(!tree.nodes.contains_key(&tag));
        assert_eq!(
            tag_store::tag_lifecycle_state(tag),
            tag_store::TagLifecycleState::Invalidated
        );

        let compact_meta = tag_store::get(tag).expect("invalidated compact metadata is kept");
        let msg = tb_lite_check(tag, tag, &compact_meta, addr, 1, AliasAccessKind::Read)
            .expect("exact compacted stale tag should report");
        assert!(msg.contains("reason=TB_LITE_INVALIDATED"));
        assert!(msg.contains(&format!("tag={tag}")));
    }

    #[test]
    fn compact_invalidated_keeps_parent_with_live_child() {
        std::env::set_var("RZ_TB_COMPACT_INVALIDATED_TAGS", "1");

        let parent = next_test_tag();
        let child = next_test_tag();
        let addr = test_addr(parent);
        tag_store::insert(parent, test_meta(addr, PtrKind::RefMut, 0));
        tag_store::insert(child, test_meta(addr, PtrKind::RefShared, parent));

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(
            parent,
            0,
            addr,
            BorrowKind::Unique,
            TbPerm::Disabled,
        ));
        tree.insert_node(test_node(
            child,
            parent,
            addr,
            BorrowKind::Shared,
            TbPerm::Frozen,
        ));

        let removed = tb_compact_unreachable_invalidated_subtree(&mut tree, parent, &[]);
        assert_eq!(removed, 0);
        assert!(tree.nodes.contains_key(&parent));
        assert!(tree.nodes.contains_key(&child));
        assert_eq!(
            tag_store::tag_lifecycle_state(parent),
            tag_store::TagLifecycleState::Active
        );
    }

    #[test]
    fn compaction_dispatcher_prunes_dead_shared_sibling() {
        std::env::set_var("RZ_TB_COMPACT_INVALIDATED_TAGS", "1");

        let unique = next_test_tag();
        let shared = next_test_tag();
        let addr = test_addr(unique);
        tag_store::insert(unique, test_meta(addr, PtrKind::RefMut, 0));
        tag_store::insert(shared, test_meta(addr, PtrKind::RefShared, 0));
        tag_store::retain_local_holder(shared);
        tag_store::mark_escaped(shared);

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(
            unique,
            0,
            addr,
            BorrowKind::Unique,
            TbPerm::Active,
        ));
        tree.insert_node(test_node(
            shared,
            0,
            addr,
            BorrowKind::Shared,
            TbPerm::Disabled,
        ));

        let removed = tb_compact_invalidated_roots(&mut tree, &[shared], true);
        assert_eq!(removed, 1);

        assert!(tree.nodes.contains_key(&unique));
        assert!(!tree.nodes.contains_key(&shared));
        assert_eq!(
            tag_store::tag_lifecycle_state(shared),
            tag_store::TagLifecycleState::Invalidated
        );

        let shared_meta = tag_store::get(shared).expect("compacted shared tag metadata is kept");
        let msg = tb_lite_check(shared, shared, &shared_meta, addr, 1, AliasAccessKind::Read)
            .expect("exact stale shared tag should still report");
        assert!(msg.contains("reason=TB_LITE_INVALIDATED"));
        assert!(msg.contains(&format!("tag={shared}")));
        assert!(tag_store::release_local_holder(shared));
    }

    #[test]
    fn range_index_finds_only_overlapping_nodes() {
        let left = next_test_tag();
        let right = next_test_tag();
        let left_addr = test_addr(left);
        let right_addr = left_addr.saturating_add(TB_RANGE_INDEX_BUCKET_SIZE * 4);

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(
            left,
            0,
            left_addr,
            BorrowKind::Shared,
            TbPerm::Frozen,
        ));
        tree.insert_node(test_node(
            right,
            0,
            right_addr,
            BorrowKind::Shared,
            TbPerm::Frozen,
        ));

        let left_hits = tree.overlapping_indexed_tags(left_addr, 1);
        assert!(left_hits.contains(&left));
        assert!(!left_hits.contains(&right));

        let right_hits = tree.overlapping_indexed_tags(right_addr, 1);
        assert!(right_hits.contains(&right));
        assert!(!right_hits.contains(&left));
    }

    #[test]
    fn missed_foreign_read_freezes_uncovered_lazy_perm() {
        let tag = next_test_tag();
        let addr = test_addr(tag);

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(tag, 0, addr, BorrowKind::Unique, TbPerm::Active));

        tree.foreign_read_epoch = tree.foreign_read_epoch.saturating_add(1);
        tb_sync_node_lazy_reads(&mut tree, tag);

        let node = tree.nodes.get(&tag).expect("test node exists");
        assert_eq!(node.perm, TbPerm::Active);
        assert_eq!(node.lazy_perm, TbPerm::Frozen);
        assert_eq!(node.lazy_read_epoch, tree.foreign_read_epoch);
    }

    #[test]
    fn live_same_slot_unique_descendant_is_detected() {
        let parent = next_test_tag();
        let child = next_test_tag();
        let addr = test_addr(parent);

        let mut tree = TbAllocState::default();
        tree.insert_node(test_node(
            parent,
            0,
            addr,
            BorrowKind::Unique,
            TbPerm::Active,
        ));
        tree.insert_node(test_node(
            child,
            parent,
            addr,
            BorrowKind::Unique,
            TbPerm::Active,
        ));

        assert!(tb_has_live_same_slot_unique_descendant(
            &tree, parent, addr, 1
        ));
    }
}
