use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{allocs, find_alloc_containing, rz_sb_suppressed, tags, PtrKind, TagMeta};

use super::{stacked_borrows_lite::StackedBorrowsLiteModel, AliasAccessKind, AliasModel};

pub(crate) struct TreeBorrowsLiteModel;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BorrowKind {
    Shared,
    Unique,
}

#[derive(Clone, Debug)]
struct TbNode {
    tag: u64,
    parent: u64,
    kind: BorrowKind,
    start: usize,
    len: usize,
    alive: bool,
}

#[derive(Default)]
struct TbAllocState {
    nodes: HashMap<u64, TbNode>,
}

static TB_STATE: OnceLock<Mutex<HashMap<usize, TbAllocState>>> = OnceLock::new();

fn tb_state() -> &'static Mutex<HashMap<usize, TbAllocState>> {
    TB_STATE.get_or_init(|| Mutex::new(HashMap::new()))
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
fn rz_tb_strict_creation_enabled() -> bool {
    std::env::var("RZ_TB_STRICT_CREATE")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

impl AliasModel for TreeBorrowsLiteModel {
    fn name(&self) -> &'static str {
        "tb_lite"
    }

    fn on_alloc_state_change(&self, base_addr: usize, new_live: bool) {
        let sb = StackedBorrowsLiteModel;
        sb.on_alloc_state_change(base_addr, new_live);
        if !new_live && rz_tb_lite_enabled() {
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
        let sb = StackedBorrowsLiteModel;
        if let Some(msg) = sb.validate_ref_creation(
            pointee_addr,
            new_kind,
            parent_tag,
            alias_exempt,
            bounds_len,
        ) {
            return Some(msg);
        }
        tb_lite_validate_ref_creation(pointee_addr, new_kind, parent_tag, alias_exempt, bounds_len)
    }

    fn on_tag_created(&self, tag: u64, tmeta: &TagMeta) {
        let sb = StackedBorrowsLiteModel;
        sb.on_tag_created(tag, tmeta);
        tb_lite_on_tag_created(tag, tmeta);
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
        let sb = StackedBorrowsLiteModel;
        if let Some(msg) = sb.check_access(sb_tag, orig_tag, tmeta, addr, size, access) {
            return Some(msg);
        }
        tb_lite_check(sb_tag, tmeta, addr, size, access)
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
    // Default TB-lite mode avoids creation-time hard rejects because optimized MIR often
    // keeps prior ref tags "alive" longer than source-level lifetimes, which can yield
    // false positives on tight reborrow loops in real crates (e.g., serde_json parser hot paths).
    // Access-time invalidation/checks remain active.
    if !rz_tb_strict_creation_enabled() {
        return None;
    }
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
    let parent_ref = if parent_tag != 0 {
        let tmap = tags().lock().unwrap();
        tb_lite_find_ref_ancestor_tag(&tmap, parent_tag)
    } else {
        None
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
        if !node.alive {
            continue;
        }
        if node.kind != BorrowKind::Unique {
            continue;
        }
        if !tb_ranges_overlap(pointee_addr, new_len, node.start, node.len) {
            continue;
        }

        // Allow creation if the overlap is within the same lineage.
        let same_lineage = node.tag == parent_ref
            || tb_is_ancestor(&tree.nodes, node.tag, parent_ref)
            || tb_is_ancestor(&tree.nodes, parent_ref, node.tag);
        if same_lineage {
            continue;
        }

        return Some(format!(
            "TB_LITE reborrow conflict: create RefMut [0x{:x},0x{:x}) overlaps active tag={} kind={:?} [0x{:x},0x{:x})",
            pointee_addr,
            new_end,
            node.tag,
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
        _ => return,
    };

    let parent = if tmeta.parent == 0 {
        0
    } else {
        let tmap = tags().lock().unwrap();
        tb_lite_find_ref_ancestor_tag(&tmap, tmeta.parent).unwrap_or(0)
    };

    let base = tb_base_for_addr(tmeta.pointee_addr);
    let node = TbNode {
        tag,
        parent,
        kind,
        start: tmeta.pointee_addr,
        len: tb_effective_len(tmeta.bounds_len),
        alive: true,
    };

    let mut all = tb_state().lock().unwrap();
    let tree = all.entry(base).or_default();
    tree.nodes.insert(tag, node.clone());

    // Eager invalidation for unique creation keeps the tree state monotonic and
    // catches "two overlapping unique sibling" constructions immediately.
    if kind == BorrowKind::Unique {
        let victim_tags: Vec<u64> = tree
            .nodes
            .values()
            .filter(|n| n.alive && n.tag != tag)
            .filter(|n| tb_ranges_overlap(node.start, node.len, n.start, n.len))
            .filter(|n| {
                !tb_is_ancestor(&tree.nodes, n.tag, tag) && !tb_is_ancestor(&tree.nodes, tag, n.tag)
            })
            .map(|n| n.tag)
            .collect();
        for victim in victim_tags {
            if let Some(n) = tree.nodes.get_mut(&victim) {
                n.alive = false;
            }
        }
    }
}

fn tb_lite_check(
    sb_tag: u64,
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

    let Some(node) = tree.nodes.get(&sb_tag).cloned() else {
        // Best-effort: missing node means missing model metadata, not definite UB.
        return None;
    };
    let dump = if rz_tb_dump_enabled() {
        tb_dump(tree, sb_tag, addr, size, access)
    } else {
        String::new()
    };
    if !node.alive {
        // Best-effort compromise: optimized MIR often keeps short-lived reference tags
        // around in a way that over-approximates lifetimes. Emitting TB invalidation
        // errors on Ref* accesses is too noisy on real crates, so keep enforcement on
        // raw accesses while still updating TB state on writes.
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
            return None;
        }
        let mut msg = format!(
            "{} via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_INVALIDATED",
            tb_access_name(access),
            sb_tag,
            addr,
            size,
            tmeta.kind
        );
        msg.push_str(&dump);
        return Some(msg);
    }

    match access {
        AliasAccessKind::Read => {
            None
        }
        AliasAccessKind::Write => {
            if node.kind != BorrowKind::Unique {
                let mut msg = format!(
                    "WRITE via tag={} addr=0x{:x} size={} kind={:?}\nreason=TB_LITE_NON_UNIQUE_WRITE",
                    sb_tag, addr, size, tmeta.kind
                );
                msg.push_str(&dump);
                return Some(msg);
            }

            // Tree-borrows-style "use": writing through this unique branch invalidates
            // overlapping nodes outside its ancestor chain.
            let victims: Vec<u64> = tree
                .nodes
                .values()
                .filter(|n| n.alive && n.tag != sb_tag)
                .filter(|n| tb_ranges_overlap(addr, size, n.start, n.len))
                .filter(|n| !tb_is_ancestor(&tree.nodes, n.tag, sb_tag))
                .map(|n| n.tag)
                .collect();
            for victim in victims {
                if let Some(n) = tree.nodes.get_mut(&victim) {
                    n.alive = false;
                }
            }
            None
        }
    }
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
            "  tag={} parent={} kind={:?} alive={} range=[0x{:x},0x{:x})\n",
            n.tag,
            n.parent,
            n.kind,
            n.alive,
            n.start,
            n.start.saturating_add(n.len)
        ));
    }
    out.push_str("-- end tb-lite dump --\n");
    out
}

#[inline]
fn tb_effective_len(bounds_len: usize) -> usize {
    if bounds_len == 0 { 1 } else { bounds_len }
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

fn tb_lite_find_ref_ancestor_tag(tmap: &HashMap<u64, TagMeta>, mut tag: u64) -> Option<u64> {
    for _ in 0..32 {
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

#[inline]
fn tb_is_ancestor(nodes: &HashMap<u64, TbNode>, ancestor: u64, mut tag: u64) -> bool {
    if ancestor == tag {
        return true;
    }
    for _ in 0..64 {
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
