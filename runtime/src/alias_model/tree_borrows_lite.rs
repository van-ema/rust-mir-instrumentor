use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use crate::{
    allocs, append_location_if_enabled, find_alloc_containing, ret_tags, rz_sb_suppressed,
    rz_violation, tags, PtrKind, TagMeta,
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
    start: usize,
    len: usize,
    alive: bool,
    protected: bool,
}

#[derive(Default)]
struct TbAllocState {
    nodes: HashMap<u64, TbNode>,
}

#[derive(Default)]
struct TbProtectorFrame {
    callee_id: u64,
    pending_parent_tags: Vec<u64>,
    protected_tags: Vec<u64>,
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

    fn on_call_arg_taken(&self, callee_id: u64, parent_tag: u64) {
        tb_lite_on_call_arg_taken(callee_id, parent_tag);
    }

    fn on_call_exit(&self, callee_id: u64) {
        tb_lite_on_call_exit(callee_id);
    }

    fn find_ref_ancestor_tag(&self, tmap: &HashMap<u64, TagMeta>, tag: u64) -> Option<u64> {
        tb_lite_find_ref_ancestor_tag(tmap, tag)
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

fn tb_lite_on_call_arg_taken(callee_id: u64, parent_tag: u64) {
    if !rz_tb_lite_enabled() || parent_tag == 0 {
        return;
    }
    let mut frames = tb_protector_frames().lock().unwrap();
    match frames.last_mut() {
        Some(top) if top.callee_id == callee_id => {
            top.pending_parent_tags.push(parent_tag);
        }
        _ => {
            frames.push(TbProtectorFrame {
                callee_id,
                pending_parent_tags: vec![parent_tag],
                protected_tags: Vec::new(),
            });
        }
    }
}

fn tb_lite_on_call_exit(callee_id: u64) {
    if !rz_tb_lite_enabled() {
        return;
    }

    let popped = {
        let mut frames = tb_protector_frames().lock().unwrap();
        frames
            .iter()
            .rposition(|f| f.callee_id == callee_id)
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
        .filter(|((ret_callee_id, _addr), _tag)| *ret_callee_id == callee_id)
        .map(|((_ret_callee_id, _addr), tag)| *tag)
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
        if let Some(node) = tree.nodes.get_mut(&tag) {
            node.protected = false;
            if !returned_tags.contains(&tag) {
                tb_disable_node(node);
            }
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
            .find(|n| tb_is_live_node(n) && n.protected)
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
    // Creation-time mutable reborrow conflicts are always checked in TB-lite.
    if !matches!(new_kind, PtrKind::RefShared | PtrKind::RefMut) {
        return None;
    }

    // In TB-lite, only mutable reborrows are rejected at creation time.
    // Shared reborrows are always admitted and may freeze behavior at access time.
    if !matches!(new_kind, PtrKind::RefMut) {
        return None;
    }

    let new_len = tb_effective_len(bounds_len);
    let new_end = pointee_addr.saturating_add(new_len);
    let base = tb_base_for_addr(pointee_addr);
    let (parent_ref, parent_epoch) = if parent_tag != 0 {
        let tmap = tags().lock().unwrap();
        let pref = tb_lite_find_ref_ancestor_tag(&tmap, parent_tag);
        let pep = pref
            .and_then(|t| tmap.get(&t).map(|m| m.alloc_epoch))
            .unwrap_or(0);
        (pref, pep)
    } else {
        (None, 0)
    };

    let Some(parent_ref) = parent_ref else {
        // Without a ref ancestor we cannot reason about branch lineage reliably.
        // Skip creation-time rejection and let access-time invalidation/checks decide.
        return None;
    };

    let state = tb_state().lock().unwrap();
    let Some(tree) = state.get(&base) else {
        return None;
    };
    if !tree.nodes.contains_key(&parent_ref) {
        // Parent lineage is not materialized in TB state (best-effort metadata loss).
        // Be conservative and defer to access-time checks rather than rejecting now.
        return None;
    }

    for node in tree.nodes.values() {
        if !tb_is_live_node(node) {
            continue;
        }
        if node.kind != BorrowKind::Unique {
            continue;
        }
        if !tb_ranges_overlap(pointee_addr, new_len, node.start, node.len) {
            continue;
        }
        if parent_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != parent_epoch {
            // Different allocation epoch at same base address (e.g. stack slot reuse).
            // Ignore old-lifetime nodes to avoid stale-lineage conflicts.
            continue;
        }

        // Allow creation if the overlap is within the same lineage.
        let same_lineage = node.tag == parent_ref
            || tb_is_ancestor(&tree.nodes, node.tag, parent_ref)
            || tb_is_ancestor(&tree.nodes, parent_ref, node.tag);
        if same_lineage {
            continue;
        }

        // TB-lite policy:
        // - for non-protected overlaps, defer to access-time transitions/violations;
        // - reject at creation only when this would overlap an active protected unique.
        //
        // This avoids false positives in safe code paths that transiently create overlapping
        // mutable refs but never perform an invalid protected/foreign access.
        if !node.protected {
            continue;
        }

        return Some(format!(
            "TB_LITE reborrow conflict: create RefMut [0x{:x},0x{:x}) parent_tag={} parent_ref={} parent_epoch={} overlaps active protected tag={} active_epoch={} kind={:?} [0x{:x},0x{:x})",
            pointee_addr,
            new_end,
            parent_tag,
            parent_ref,
            parent_epoch,
            node.tag,
            node.alloc_epoch,
            node.kind,
            node.start,
            node.start.saturating_add(node.len)
        ));
    }

    None
}

fn tb_lite_on_tag_created(tag: u64, tmeta: &TagMeta) {
    if !rz_tb_lite_enabled() || tmeta.alias_exempt {
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
        match kind {
            BorrowKind::Shared | BorrowKind::Unique => {
                tb_lite_find_materialized_ref_ancestor_tag(&tmap, &tree.nodes, tmeta.parent)
                    .or_else(|| tb_lite_find_ref_ancestor_tag(&tmap, tmeta.parent))
                    .unwrap_or(tmeta.parent)
            }
            BorrowKind::RawConst | BorrowKind::RawMut => {
                tb_lite_find_ref_ancestor_tag(&tmap, tmeta.parent).unwrap_or(tmeta.parent)
            }
        }
    };
    let protected = tb_lite_mark_protected_if_pending(tag, parent, kind);
    let node = TbNode {
        tag,
        parent,
        alloc_epoch: tmeta.alloc_epoch,
        kind,
        perm,
        start: tmeta.pointee_addr,
        len: tb_effective_len(tmeta.bounds_len),
        alive: true,
        protected,
    };
    tree.nodes.insert(tag, node.clone());

    // Eager invalidation for mutable-reference creation keeps the tree state monotonic and
    // catches "two overlapping unique sibling" constructions immediately.
    if kind == BorrowKind::Unique {
        let victim_tags: Vec<u64> = tree
            .nodes
            .values()
            .filter(|n| tb_is_live_node(n) && n.tag != tag)
            .filter(|n| tb_ranges_overlap(node.start, node.len, n.start, n.len))
            .filter(|n| {
                // Best-effort metadata can lose parent lineage on projection-heavy code paths,
                // yielding overlapping root uniques (`parent=0`) that are still used safely.
                // Do not eagerly invalidate root-vs-root siblings at creation; let access-time
                // transitions decide conflicts when/if they actually occur.
                //
                // Example:
                //   let chunk = &mut out[i..i+4];
                //   chunk[0] = ...; chunk[1] = ...;
                // With imprecise parent lowering, each per-element/per-subslice ref can appear
                // as a fresh root unique over overlapping ranges. Eager sibling invalidation here
                // would disable earlier roots immediately and report TB_LITE_INVALIDATED on valid
                // subsequent writes in the same loop.
                if node.parent == 0 && n.parent == 0 {
                    return false;
                }
                !tb_is_ancestor(&tree.nodes, n.tag, tag) && !tb_is_ancestor(&tree.nodes, tag, n.tag)
            })
            .map(|n| n.tag)
            .collect();
        for victim in victim_tags {
            if let Some(n) = tree.nodes.get_mut(&victim) {
                tb_disable_node(n);
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
fn tb_lite_mark_protected_if_pending(tag: u64, parent: u64, kind: BorrowKind) -> bool {
    if !matches!(kind, BorrowKind::Shared | BorrowKind::Unique) || parent == 0 {
        return false;
    }
    let mut frames = tb_protector_frames().lock().unwrap();
    let Some(top) = frames.last_mut() else {
        return false;
    };
    let Some(pos) = top.pending_parent_tags.iter().position(|p| *p == parent) else {
        return false;
    };
    top.pending_parent_tags.swap_remove(pos);
    top.protected_tags.push(tag);
    true
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

    let base = tb_base_for_addr(addr);
    let mut all = tb_state().lock().unwrap();
    let Some(tree) = all.get_mut(&base) else {
        return None;
    };

    // Use the original tag when TB tracked it (notably raw tags); otherwise fall back to
    // the nearest-ref tag used by the generic fast path.
    let access_tag = if tree.nodes.contains_key(&orig_tag) {
        orig_tag
    } else {
        sb_tag
    };

    let Some(node) = tree.nodes.get(&access_tag).cloned() else {
        // Best-effort: missing node means missing model metadata, not definite UB.
        return None;
    };
    if tmeta.alloc_epoch != 0 && node.alloc_epoch != 0 && node.alloc_epoch != tmeta.alloc_epoch {
        // Tag metadata and TB node disagree on epoch; treat as stale model state and skip.
        return None;
    }
    let dump = if rz_tb_dump_enabled() {
        tb_dump(tree, access_tag, addr, size, access)
    } else {
        String::new()
    };
    if !tb_is_live_node(&node) {
        if matches!(tmeta.kind, PtrKind::RefMut) {
            if let Some(descendants) =
                tb_lite_reactivatable_descendants(tree, access_tag, addr, size, tmeta.alloc_epoch)
            {
                for tag in descendants {
                    if let Some(n) = tree.nodes.get_mut(&tag) {
                        tb_disable_node(n);
                    }
                }
                if let Some(n) = tree.nodes.get_mut(&access_tag) {
                    n.perm = TbPerm::Active;
                    n.alive = true;
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

    // Apply a TB-lite transition to all overlapping nodes.
    // `child` means the access goes through this node's lineage (node is an ancestor
    // of the accessing tag, including itself). `foreign` means all other overlaps.
    let overlapping_tags: Vec<u64> = tree
        .nodes
        .values()
        .filter(|n| tb_is_live_node(n))
        .filter(|n| tb_ranges_overlap(addr, size, n.start, n.len))
        .filter(|n| {
            if tmeta.alloc_epoch != 0 && n.alloc_epoch != 0 && n.alloc_epoch != tmeta.alloc_epoch {
                return false;
            }
            true
        })
        .map(|n| n.tag)
        .collect();

    let mut updates: Vec<(u64, TbPerm)> = Vec::new();
    for tag in overlapping_tags {
        let Some(n) = tree.nodes.get(&tag).cloned() else {
            continue;
        };
        let child = tb_is_ancestor(&tree.nodes, n.tag, access_tag);

        let next = match (access, child, n.perm, n.protected) {
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
            (AliasAccessKind::Read, false, TbPerm::Reserved { .. }, _) => n.perm,
            (AliasAccessKind::Read, false, TbPerm::Active, true) => TbPerm::Disabled,
            (AliasAccessKind::Read, false, TbPerm::Active, false) => TbPerm::Frozen,
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
            (AliasAccessKind::Write, true, TbPerm::Frozen, _) => {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_FROZEN_WRITE",
                    access_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }
            (AliasAccessKind::Write, true, TbPerm::Disabled, _) => {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_DISABLED_WRITE",
                    access_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }

            // Foreign write: disable.
            (AliasAccessKind::Write, false, _, _) => TbPerm::Disabled,
        };

        if next != n.perm {
            updates.push((tag, next));
        }
    }

    if matches!(access, AliasAccessKind::Write) {
        // Keep dedicated protector diagnostic for write-through-other-tag while protected.
        if let Some(protected) = tree
            .nodes
            .values()
            .find(|n| {
                tb_is_live_node(n)
                    && n.protected
                    && n.tag != access_tag
                    && (tmeta.alloc_epoch == 0
                        || n.alloc_epoch == 0
                        || n.alloc_epoch == tmeta.alloc_epoch)
            })
            .cloned()
        {
            let touched_protected = updates
                .iter()
                .any(|(t, next)| *t == protected.tag && *next == TbPerm::Disabled);
            if touched_protected {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_PROTECTOR_CONFLICT protected_tag={} protected_kind={:?}",
                    access_tag, addr, size, tmeta.kind, protected.tag, protected.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }
        }
    }

    for (tag, next) in updates {
        if let Some(n) = tree.nodes.get_mut(&tag) {
            n.perm = next;
            n.alive = next != TbPerm::Disabled;
        }
    }

    None
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
            "  tag={} parent={} epoch={} kind={:?} perm={:?} alive={} protected={} range=[0x{:x},0x{:x})\n",
            n.tag,
            n.parent,
            n.alloc_epoch,
            n.kind,
            n.perm,
            n.alive,
            n.protected,
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
fn tb_is_live_node(n: &TbNode) -> bool {
    n.alive && n.perm != TbPerm::Disabled
}

#[inline]
fn tb_disable_node(n: &mut TbNode) {
    n.perm = TbPerm::Disabled;
    n.alive = false;
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

fn tb_lite_reactivatable_descendants(
    tree: &TbAllocState,
    access_tag: u64,
    addr: usize,
    size: usize,
    alloc_epoch: u64,
) -> Option<Vec<u64>> {
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
    if overlapping_live
        .iter()
        .all(|tag| *tag != access_tag && tb_is_ancestor(&tree.nodes, access_tag, *tag))
    {
        return Some(overlapping_live);
    }
    None
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
