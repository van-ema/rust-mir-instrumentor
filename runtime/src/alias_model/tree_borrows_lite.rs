use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use crate::{
    allocs, append_location_if_enabled, find_alloc_containing, mut_arg_ret_leaf_shadows,
    mut_arg_ret_tags, ret_leaf_shadows, ret_tags, rz_sb_suppressed, rz_violation, tag_store, tags,
    PtrKind, TagMeta,
};

use super::{AliasAccessKind, AliasModel};

pub(crate) struct TreeBorrowsLiteModel;

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
    Disabled,
}

#[derive(Clone, Debug)]
struct TbNode {
    tag: u64,
    parent: u64,
    alloc_epoch: u64,
    kind: BorrowKind,
    perm: TbPerm,
    lazy_perm: TbPerm,
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

fn tb_state() -> &'static Mutex<HashMap<usize, TbAllocState>> {
    TB_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tb_protector_frames() -> &'static Mutex<Vec<TbProtectorFrame>> {
    TB_PROTECTOR_FRAMES.get_or_init(|| Mutex::new(Vec::new()))
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
fn rz_tb_no_precise_interior_mut_enabled() -> bool {
    std::env::var("RZ_TB_NO_PRECISE_INTERIOR_MUT")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[cfg(feature = "runtime_lineage_repair")]
#[inline]
fn rz_tb_runtime_lineage_repair_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("RZ_DISABLE_RUNTIME_LINEAGE_REPAIR")
            .ok()
            .is_some_and(|v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

#[cfg(not(feature = "runtime_lineage_repair"))]
#[inline(always)]
fn rz_tb_runtime_lineage_repair_enabled() -> bool {
    false
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
        tb_lite_on_mut_arg_ret_export(tag, addr);
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
/// shadowed by nested `&mut self` calls are restored, and non-returned protected tags are
/// disabled at protector end.
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

    let returned_tags: HashSet<u64> = ret_tags()
        .lock()
        .unwrap()
        .iter()
        .filter(|((ret_thread_id, ret_callee_id, _addr), _tag)| {
            *ret_thread_id == thread_id && *ret_callee_id == callee_id
        })
        .map(|((_ret_thread_id, _ret_callee_id, _addr), tag)| *tag)
        .chain(
            ret_leaf_shadows()
                .lock()
                .unwrap()
                .iter()
                .filter(|((ret_thread_id, ret_callee_id, _leaf_index), _shadow)| {
                    *ret_thread_id == thread_id && *ret_callee_id == callee_id
                })
                .map(
                    |(
                        (_ret_thread_id, _ret_callee_id, _leaf_index),
                        (tag, _ref_ancestor, _export_parent, _export_parent_recovered),
                    )| *tag,
                ),
        )
        .chain(
            mut_arg_ret_tags()
                .lock()
                .unwrap()
                .iter()
                .filter(
                    |((ret_thread_id, ret_callee_id, _arg_index, _addr), _tag)| {
                        *ret_thread_id == thread_id && *ret_callee_id == callee_id
                    },
                )
                .map(|((_ret_thread_id, _ret_callee_id, _arg_index, _addr), tag)| *tag),
        )
        .chain(
            mut_arg_ret_leaf_shadows()
                .lock()
                .unwrap()
                .iter()
                .filter(
                    |((ret_thread_id, ret_callee_id, _arg_index, _addr, _leaf_index), _shadow)| {
                        *ret_thread_id == thread_id && *ret_callee_id == callee_id
                    },
                )
                .map(
                    |(
                        (_ret_thread_id, _ret_callee_id, _arg_index, _addr, _leaf_index),
                        (tag, _ref_ancestor, _export_parent, _export_parent_recovered),
                    )| { *tag },
                ),
        )
        .collect();
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
            let descendant_tags: Vec<u64> = tree
                .nodes
                .values()
                .filter(|n| n.tag != tag && tb_is_ancestor(&tree.nodes, tag, n.tag))
                .map(|n| n.tag)
                .collect();
            for descendant in descendant_tags {
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
        if let Some(node) = tree.nodes.get_mut(&tag) {
            node.protected = false;
            if !returned_tags.contains(&tag) {
                tb_disable_node_for_protector_end(node);
            }
        }
        tb_unshadow_same_slot_protected_unique_ancestors(
            tree,
            tag,
            tmeta.pointee_addr,
            tb_effective_len(tmeta.bounds_len),
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
        let descendant_tags: Vec<u64> = tree
            .nodes
            .values()
            .filter(|n| n.tag != tag && tb_is_ancestor(&tree.nodes, tag, n.tag))
            .map(|n| n.tag)
            .collect();
        for descendant in descendant_tags {
            if let Some(node) = tree.nodes.get_mut(&descendant) {
                tb_disable_node_for_protector_end(node);
            }
        }
        if let Some(node) = tree.nodes.get_mut(&tag) {
            tb_disable_node_for_protector_end(node);
        }
    }
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
    if !rz_tb_lite_enabled()
        || rz_tb_no_precise_interior_mut_enabled()
        || alias_exempt
        || rz_sb_suppressed()
    {
        return None;
    }
    if !matches!(new_kind, PtrKind::RefShared | PtrKind::RefMut) {
        return None;
    }

    let new_len = tb_effective_len(bounds_len);
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
    let perm = match kind {
        BorrowKind::Unique => TbPerm::Reserved { conflicted: false },
        BorrowKind::RawMut => TbPerm::Active,
        BorrowKind::Shared | BorrowKind::RawConst => TbPerm::Frozen,
    };
    let mut all = tb_state().lock().unwrap();
    let tree = all.entry(base).or_default();
    let parent = if tmeta.parent == 0 {
        0
    } else {
        let tmap = tags().lock().unwrap();
        let projected_helper_ref_parent = matches!(kind, BorrowKind::Shared | BorrowKind::Unique)
            && (tmeta.lineage_hint & 0b0000_0100) != 0
            && tmap.get(&tmeta.parent).is_some_and(|meta| {
                meta.parent == 0 && matches!(meta.kind, PtrKind::RawConst | PtrKind::RawMut)
            });
        let effective_parent = if projected_helper_ref_parent {
            tb_lite_find_ref_ancestor_tag(&tmap, tmeta.parent).unwrap_or(0)
        } else {
            tmeta.parent
        };
        match kind {
            BorrowKind::Shared | BorrowKind::Unique => {
                tb_lite_find_materialized_ref_ancestor_tag(&tmap, &tree.nodes, effective_parent)
                    .or_else(|| tb_lite_find_ref_ancestor_tag(&tmap, effective_parent))
                    .unwrap_or(effective_parent)
            }
            BorrowKind::RawConst | BorrowKind::RawMut => {
                tb_lite_find_ref_ancestor_tag(&tmap, effective_parent).unwrap_or(effective_parent)
            }
        }
    };
    let protected = tb_lite_mark_protected_if_pending(tag, parent, kind);
    tb_lite_mark_inplace_protected_if_pending(tag, parent, tmeta.parent, tmeta.pointee_addr, kind);
    let node = TbNode {
        tag,
        parent,
        alloc_epoch: tmeta.alloc_epoch,
        kind,
        perm,
        lazy_perm: perm,
        start: tmeta.pointee_addr,
        len: tb_effective_len(tmeta.bounds_len),
        extra_ranges: Vec::new(),
        alive: true,
        protected,
        protector_shadow_depth: 0,
        poisoned_by_protector_end: false,
    };
    tree.nodes.insert(tag, node.clone());
    let returned_carrier_reroot = (tmeta.lineage_hint & 0b1000) != 0;
    if returned_carrier_reroot {
        let stack_like_root_ref =
            parent == 0 && matches!(kind, BorrowKind::Shared | BorrowKind::Unique) && {
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
            if let Some(n) = tree.nodes.get_mut(&victim) {
                n.perm = TbPerm::Frozen;
                n.lazy_perm = TbPerm::Frozen;
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

/// Re-enable a family exported through the mut-arg-ret side channel after call-exit teardown.
///
/// Today the return-side hook ordering can still publish the exported tag after `FnExit` has
/// already released the callee's protector frame. When that happens, the exported family is the
/// caller-visible survivor and must be live again before the next call boundary validates it.
fn tb_lite_on_mut_arg_ret_export(tag: u64, addr: usize) {
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
    node.protected = false;
    node.protector_shadow_depth = 0;
    node.poisoned_by_protector_end = false;
    node.alive = true;
    if matches!(node.perm, TbPerm::Disabled) {
        node.perm = match node.kind {
            BorrowKind::Unique | BorrowKind::RawMut => TbPerm::Active,
            BorrowKind::Shared | BorrowKind::RawConst => TbPerm::Frozen,
        };
    }
    if matches!(node.lazy_perm, TbPerm::Disabled) {
        node.lazy_perm = node.perm;
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

    let Some(tmeta) = tags().lock().unwrap().get(&tag).copied() else {
        return;
    };
    let base = tb_base_for_addr(tmeta.pointee_addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return;
    };
    let Some(node) = tree.nodes.get_mut(&tag) else {
        return;
    };
    tb_disable_node(node);
}

fn tb_lite_check(
    sb_tag: u64,
    orig_tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
    access: AliasAccessKind,
) -> Option<String> {
    if !rz_tb_lite_enabled()
        || rz_tb_no_precise_interior_mut_enabled()
        || tmeta.alias_exempt
        || rz_sb_suppressed()
    {
        return None;
    }
    if !matches!(
        tmeta.kind,
        PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
    ) {
        return None;
    }

    let base = tb_base_for_addr(addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return None;
    };

    // Use the original tag when TB tracked it (notably raw tags); otherwise fall back to
    // the nearest-ref tag used by the generic fast path.
    let mut access_tag = if tree.nodes.contains_key(&orig_tag) {
        orig_tag
    } else {
        sb_tag
    };

    let Some(mut node) = tree.nodes.get(&access_tag).cloned() else {
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
        if rz_tb_runtime_lineage_repair_enabled() {
            if let Some(recovered_tag) = tb_lite_recover_same_place_live_sibling(
                tree,
                access_tag,
                &node,
                addr,
                size,
                tmeta.alloc_epoch,
            ) {
                access_tag = recovered_tag;
                if let Some(recovered_node) = tree.nodes.get(&access_tag).cloned() {
                    node = recovered_node;
                    access_lineage = tb_collect_lineage(&tree.nodes, access_tag);
                }
            }
        }
    }
    if !tb_is_live_node(&node) {
        if matches!(tmeta.kind, PtrKind::RefMut) {
            if let Some(descendants) =
                tb_lite_reactivatable_same_lineage(tree, access_tag, addr, size, tmeta.alloc_epoch)
            {
                for tag in descendants {
                    if let Some(n) = tree.nodes.get_mut(&tag) {
                        tb_disable_node(n);
                    }
                }
                if let Some(n) = tree.nodes.get_mut(&access_tag) {
                    n.perm = TbPerm::Active;
                    n.alive = true;
                    n.poisoned_by_protector_end = false;
                }
                return None;
            }
        }
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

    // Apply a TB-lite transition to all nodes of the allocation.
    // For locations outside the node's currently accessed ranges, `lazy_perm`
    // approximates the "future initial permission" from the TB state machine.
    let candidate_tags: Vec<u64> = tree
        .nodes
        .values()
        .filter(|n| {
            tb_is_live_node(n)
                || (matches!(n.perm, TbPerm::Disabled)
                    && matches!(n.kind, BorrowKind::Unique)
                    && !(matches!(access, AliasAccessKind::Read)
                        && matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RawConst))
                    && tb_is_ancestor(&tree.nodes, n.tag, access_tag))
        })
        .filter(|n| {
            if tmeta.alloc_epoch != 0 && n.alloc_epoch != 0 && n.alloc_epoch != tmeta.alloc_epoch {
                return false;
            }
            true
        })
        .map(|n| n.tag)
        .collect();

    let mut updates: Vec<(u64, TbPerm, bool)> = Vec::new();
    let mut newly_accessed_ranges: Vec<u64> = Vec::new();
    for tag in candidate_tags {
        let Some(n) = tree.nodes.get(&tag).cloned() else {
            continue;
        };
        let child = tb_lineage_contains(&access_lineage, n.tag);
        let child_unique_ref_ancestor =
            child && n.tag != access_tag && matches!(n.kind, BorrowKind::Unique);
        let covered = tb_node_overlaps(&n, addr, size);
        let old_perm = if covered { n.perm } else { n.lazy_perm };

        let next = match (access, child, old_perm, tb_protector_active(&n)) {
            // Child/local read: everything except Disabled is unchanged.
            (AliasAccessKind::Read, true, TbPerm::Disabled, _) => {
                if tb_has_live_unique_lineage_ancestor(
                    &tree.nodes,
                    &access_lineage,
                    addr,
                    size,
                    tmeta.alloc_epoch,
                ) {
                    n.perm
                } else {
                    let mut msg = format!(
                        "READ via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_DISABLED_ANCESTOR ancestor_tag={}",
                        access_tag, addr, size, tmeta.kind, n.tag
                    );
                    msg.push_str(&dump);
                    return Some(msg);
                }
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
            (AliasAccessKind::Read, false, TbPerm::Frozen, _) => TbPerm::Frozen,
            (AliasAccessKind::Read, false, TbPerm::Disabled, _) => TbPerm::Disabled,

            // Child/local write:
            // - Reserved(conflicted) while protected is UB (2-phase noalias violation)
            // - Reserved/Active activate to Active
            // - Frozen/Disabled cannot be written through
            (AliasAccessKind::Write, true, _, _)
                if child_unique_ref_ancestor && matches!(tmeta.kind, PtrKind::RefMut) =>
            {
                if n.start == addr && n.len <= tb_effective_len(size) {
                    TbPerm::Disabled
                } else {
                    old_perm
                }
            }
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
                let same_family_refmut_or_rawmut = matches!(tmeta.kind, PtrKind::RefMut | PtrKind::RawMut)
                    && tb_has_live_unique_lineage_ancestor(
                        &tree.nodes,
                        &access_lineage,
                        addr,
                        size,
                        tmeta.alloc_epoch,
                    )
                    && tb_only_same_family_overlap(
                        &tree.nodes,
                        &access_lineage,
                        addr,
                        size,
                        tmeta.alloc_epoch,
                    );
                let rawmut_uncovered = matches!(tmeta.kind, PtrKind::RawMut) && !covered;
                if same_family_refmut_or_rawmut || rawmut_uncovered {
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
            .nodes
            .values()
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

    let access_len = tb_effective_len(size);
    for (tag, next, covered) in updates {
        if let Some(n) = tree.nodes.get_mut(&tag) {
            if covered {
                n.perm = next;
                n.alive = next != TbPerm::Disabled;
                if next != TbPerm::Disabled {
                    n.poisoned_by_protector_end = false;
                }
            } else {
                n.lazy_perm = next;
            }
        }
    }
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
            }
        }
    }

    None
}

fn tb_lite_recover_same_place_live_sibling(
    tree: &TbAllocState,
    dead_tag: u64,
    dead_node: &TbNode,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> Option<u64> {
    if !rz_tb_runtime_lineage_repair_enabled() {
        return None;
    }

    let access_len = tb_effective_len(size);
    tree.nodes
        .values()
        .filter(|n| n.tag != dead_tag)
        .filter(|n| tb_is_live_node(n))
        .filter(|n| n.kind == dead_node.kind)
        .filter(|n| n.parent == dead_node.parent)
        .filter(|n| n.start == dead_node.start && n.len == dead_node.len)
        .filter(|n| n.start == addr && n.len == access_len)
        .filter(|n| alloc_epoch == 0 || n.alloc_epoch == 0 || n.alloc_epoch == alloc_epoch)
        .map(|n| n.tag)
        .max()
}

fn tb_lite_recover_root_raw_mut_sibling_for_const_write(
    tree: &TbAllocState,
    raw_const_tag: u64,
    raw_const_node: &TbNode,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> Option<u64> {
    let access_len = tb_effective_len(size);
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
            "  tag={} parent={} epoch={} kind={:?} perm={:?} alive={} protected={} shadowed={} range=[0x{:x},0x{:x})\n",
            n.tag,
            n.parent,
            n.alloc_epoch,
            n.kind,
            n.perm,
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
fn tb_effective_len(bounds_len: usize) -> usize {
    if bounds_len == 0 {
        1
    } else {
        bounds_len
    }
}

#[inline]
fn tb_base_for_addr(addr: usize) -> usize {
    let amap = allocs().lock().unwrap();
    find_alloc_containing(&amap, addr)
        .map(|(base, _)| base)
        .unwrap_or(addr)
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
fn tb_node_overlaps(node: &TbNode, addr: usize, size: usize) -> bool {
    tb_ranges_overlap(addr, size, node.start, node.len)
        || node
            .extra_ranges
            .iter()
            .any(|(start, len)| tb_ranges_overlap(addr, size, *start, *len))
}

#[inline]
fn tb_is_live_node(n: &TbNode) -> bool {
    n.alive && n.perm != TbPerm::Disabled
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

/// Collapse a callee-exported exact-slot tag back to the nearest live `Unique` family that
/// survives helper teardown at call exit.
///
/// For nested `&mut self` helpers the newest exact-slot tag is often a temporary Shared child
/// (or a protected Unique that will be disabled at call exit). The caller must not refresh its
/// carrier anchor to an older root family like `50`, but it also must not import a dead inner
/// helper tag. Choosing the nearest live exact-slot Unique ancestor preserves the post-call
/// family that remains valid after the helper frame ends.
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

fn tb_lite_reactivatable_same_lineage(
    tree: &TbAllocState,
    access_tag: u64,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> Option<Vec<u64>> {
    let access_node = tree.nodes.get(&access_tag)?;
    if access_node.parent == 0 || !access_node.protected {
        return None;
    }
    let overlapping_live: Vec<u64> = tree
        .nodes
        .values()
        .filter(|n| tb_is_live_node(n))
        .filter(|n| tb_ranges_overlap(addr, size, n.start, n.len))
        .filter(|n| alloc_epoch == 0 || n.alloc_epoch == 0 || n.alloc_epoch == alloc_epoch)
        .map(|n| n.tag)
        .collect();
    if overlapping_live.is_empty() {
        return Some(Vec::new());
    }
    let mut descendants_to_disable = Vec::new();
    for tag in overlapping_live {
        if tag == access_tag {
            return None;
        }
        let Some(node) = tree.nodes.get(&tag) else {
            return None;
        };
        if tb_is_ancestor(&tree.nodes, tag, access_tag) {
            if !matches!(node.kind, BorrowKind::Unique)
                || matches!(
                    node.perm,
                    TbPerm::Disabled | TbPerm::Reserved { conflicted: true }
                )
            {
                return None;
            }
            continue;
        }
        if tb_is_ancestor(&tree.nodes, access_tag, tag) {
            descendants_to_disable.push(tag);
            continue;
        }
        return None;
    }
    Some(descendants_to_disable)
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

fn tb_only_same_family_overlap(
    nodes: &HashMap<u64, TbNode>,
    access_lineage: &[u64],
    addr: usize,
    size: usize,
    access_epoch: u64,
) -> bool {
    nodes.values().all(|other| {
        if !tb_is_live_node(other) || !tb_node_overlaps(other, addr, size) {
            return true;
        }
        if access_epoch != 0 && other.alloc_epoch != 0 && other.alloc_epoch != access_epoch {
            return true;
        }
        tb_lineage_contains(access_lineage, other.tag)
            || access_lineage
                .iter()
                .copied()
                .any(|ancestor| tb_is_ancestor(nodes, ancestor, other.tag))
            || access_lineage
                .iter()
                .copied()
                .any(|ancestor| ancestor != 0 && tb_is_ancestor(nodes, ancestor, other.tag))
            || tb_shares_nonroot_ancestor(nodes, access_lineage, other.tag)
    })
}

fn tb_shares_nonroot_ancestor(
    nodes: &HashMap<u64, TbNode>,
    access_lineage: &[u64],
    other_tag: u64,
) -> bool {
    let mut cur = other_tag;
    for _ in 0..nodes.len().saturating_add(1) {
        if cur == 0 {
            return false;
        }
        if access_lineage
            .iter()
            .copied()
            .any(|ancestor| ancestor == cur && ancestor != 0)
        {
            return true;
        }
        let Some(node) = nodes.get(&cur) else {
            return false;
        };
        cur = node.parent;
    }
    false
}

fn tb_has_live_unique_lineage_ancestor(
    nodes: &HashMap<u64, TbNode>,
    access_lineage: &[u64],
    addr: usize,
    size: usize,
    access_epoch: u64,
) -> bool {
    let access_end = addr.saturating_add(size);
    for &tag in access_lineage {
        let Some(node) = nodes.get(&tag) else {
            return false;
        };
        if access_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != access_epoch {
            continue;
        }
        if !tb_is_live_node(node) || !matches!(node.kind, BorrowKind::Unique) {
            continue;
        }
        let node_end = node.start.saturating_add(node.len);
        if node.len != 0 && addr >= node.start && access_end <= node_end {
            return true;
        }
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
