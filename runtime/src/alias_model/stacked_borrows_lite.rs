use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{allocs, find_alloc_containing, rz_sb_suppressed, tags, PtrKind, TagMeta};

use super::{AliasAccessKind, AliasModel};

pub(crate) struct StackedBorrowsLiteModel;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BorrowKind {
    Shared,
    Unique,
}

#[derive(Clone, Debug)]
struct BorrowEntry {
    tag: u64,
    kind: BorrowKind,
}

static BORROWS: OnceLock<Mutex<HashMap<usize, Vec<BorrowEntry>>>> = OnceLock::new();

fn borrows() -> &'static Mutex<HashMap<usize, Vec<BorrowEntry>>> {
    BORROWS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn rz_sb_lite_enabled() -> bool {
    std::env::var("RZ_SB_LITE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_sb_dump_enabled() -> bool {
    std::env::var("RZ_SB_DUMP")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

impl AliasModel for StackedBorrowsLiteModel {
    fn name(&self) -> &'static str {
        "sb_lite"
    }

    fn on_alloc_state_change(&self, base_addr: usize, new_live: bool) {
        if !new_live && rz_sb_lite_enabled() {
            borrows().lock().unwrap().remove(&base_addr);
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
        sb_lite_validate_ref_creation(pointee_addr, new_kind, parent_tag, alias_exempt, bounds_len)
    }

    fn on_tag_created(&self, tag: u64, tmeta: &TagMeta) {
        sb_lite_push(tag, tmeta);
    }

    fn find_ref_ancestor_tag(&self, tmap: &HashMap<u64, TagMeta>, tag: u64) -> Option<u64> {
        sb_lite_find_ref_ancestor_tag(tmap, tag)
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
        sb_lite_check(sb_tag, orig_tag, tmeta, addr, size, access)
    }
}

fn sb_lite_push(tag: u64, tmeta: &TagMeta) {
    if !rz_sb_lite_enabled() || tmeta.alias_exempt {
        return;
    }

    let kind = match tmeta.kind {
        PtrKind::RefShared => BorrowKind::Shared,
        PtrKind::RefMut => BorrowKind::Unique,
        _ => return,
    };

    let base = {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, tmeta.pointee_addr)
            .map(|(b, _)| b)
            .unwrap_or(tmeta.pointee_addr)
    };

    let mut bmap = borrows().lock().unwrap();
    let stack = bmap.entry(base).or_default();

    // Retagging: if we know the parent, truncate to it (invalidate younger tags).
    // For fresh unique borrows, clear the stack to invalidate all prior aliases.
    if tmeta.parent != 0 {
        if let Some(pos) = stack.iter().rposition(|entry| entry.tag == tmeta.parent) {
            stack.truncate(pos + 1);
        } else if kind == BorrowKind::Unique {
            stack.clear();
        }
    } else if kind == BorrowKind::Unique {
        stack.clear();
    }

    stack.push(BorrowEntry { tag, kind });
}

fn sb_lite_validate_ref_creation(
    pointee_addr: usize,
    new_kind: PtrKind,
    parent_tag: u64,
    alias_exempt: bool,
    bounds_len: usize,
) -> Option<String> {
    if !rz_sb_lite_enabled() || alias_exempt || rz_sb_suppressed() {
        return None;
    }
    // Keep default behavior for thin refs. We only add an early violation for mutable wide
    // reborrows (slice/str-like) where lineage has no same-base ref ancestor.
    if !matches!(new_kind, PtrKind::RefMut) || parent_tag == 0 || bounds_len == 0 {
        return None;
    }

    let base = {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, pointee_addr)
            .map(|(b, _)| b)
            .unwrap_or(pointee_addr)
    };

    let parent_ref = if parent_tag != 0 {
        let tmap = tags().lock().unwrap();
        let mut cur = parent_tag;
        let mut found: Option<(u64, PtrKind)> = None;
        for _ in 0..32 {
            let Some(tm) = tmap.get(&cur) else { break };
            if matches!(tm.kind, PtrKind::RefShared | PtrKind::RefMut) {
                let pbase = {
                    let amap = allocs().lock().unwrap();
                    find_alloc_containing(&amap, tm.pointee_addr)
                        .map(|(b, _)| b)
                        .unwrap_or(tm.pointee_addr)
                };
                if pbase == base {
                    found = Some((cur, tm.kind));
                }
                break;
            }
            if tm.parent == 0 {
                break;
            }
            cur = tm.parent;
        }
        found
    } else {
        None
    };

    let bmap = borrows().lock().unwrap();
    let stack = match bmap.get(&base) {
        Some(s) => s,
        None => return None,
    };
    let top = match stack.last() {
        Some(t) => t,
        None => return None,
    };

    // Helper for byte-range overlap checks on wide borrows.
    let ranges_overlap = |a_start: usize, a_len: usize, b_start: usize, b_len: usize| -> bool {
        if a_len == 0 || b_len == 0 {
            return false;
        }
        let a_end = a_start.saturating_add(a_len);
        let b_end = b_start.saturating_add(b_len);
        a_start < b_end && b_start < a_end
    };

    // For mutable wide reborrows, reject creating a sibling unique borrow that overlaps
    // the currently active unique top. This preserves valid disjoint split patterns and
    // catches buggy overlapping constructions (e.g. wrong lengths in split_at_mut).
    if let Some((pref_tag, _pref_kind)) = parent_ref {
        if top.kind == BorrowKind::Unique && top.tag != pref_tag {
            let tmap = tags().lock().unwrap();
            if let Some(top_tm) = tmap.get(&top.tag) {
                if ranges_overlap(
                    pointee_addr,
                    bounds_len,
                    top_tm.pointee_addr,
                    top_tm.bounds_len,
                ) {
                    return Some(format!(
                        "REBORROW mutable overlaps active unique sibling: new=[0x{:x},0x{:x}) top={}/[0x{:x},0x{:x}) parent_ref={}",
                        pointee_addr,
                        pointee_addr.saturating_add(bounds_len),
                        top.tag,
                        top_tm.pointee_addr,
                        top_tm.pointee_addr.saturating_add(top_tm.bounds_len),
                        pref_tag
                    ));
                }
            }
        }
    }

    // No same-base ref ancestor is common for raw-parts based builders.
    // Keep this conservative: only report when we can prove overlap with an
    // active unique top (otherwise we risk false positives in real crates).
    if parent_ref.is_none() && top.kind == BorrowKind::Unique {
        let tmap = tags().lock().unwrap();
        if let Some(top_tm) = tmap.get(&top.tag) {
            if ranges_overlap(
                pointee_addr,
                bounds_len,
                top_tm.pointee_addr,
                top_tm.bounds_len,
            ) {
                let parent_root = sb_lite_root_tag(&tmap, parent_tag);
                let top_root = sb_lite_root_tag(&tmap, top.tag);
                let parent_has_ref_ancestor = if parent_tag != 0 {
                    sb_lite_find_ref_ancestor_tag(&tmap, parent_tag).is_some()
                } else {
                    false
                };
                let same_range = pointee_addr == top_tm.pointee_addr
                    && bounds_len == top_tm.bounds_len;
                // Missing-parent fallback is intentionally conservative to limit false positives
                // when lineage is incomplete (e.g., wrapper types like NonNull). If roots differ,
                // report only on exact-range duplicates; partial overlaps are too noisy.
                if parent_root != top_root && !same_range {
                    return None;
                }
                return Some(format!(
                    "REBORROW mutable without same-base ref parent overlaps active unique: base=0x{base:x} new=[0x{:x},0x{:x}) top={}/[0x{:x},0x{:x}) parent_tag={} parent_root={} top_root={} parent_has_ref_ancestor={}",
                    pointee_addr,
                    pointee_addr.saturating_add(bounds_len),
                    top.tag,
                    top_tm.pointee_addr,
                    top_tm.pointee_addr.saturating_add(top_tm.bounds_len),
                    parent_tag,
                    parent_root,
                    top_root,
                    parent_has_ref_ancestor,
                ));
            }
        }
    }

    None
}

fn sb_lite_check(
    sb_tag: u64,
    orig_tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
    access: AliasAccessKind,
) -> Option<String> {
    if !rz_sb_lite_enabled() || tmeta.alias_exempt || rz_sb_suppressed() {
        return None;
    }

    if !matches!(
        tmeta.kind,
        PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
    ) {
        return None;
    }

    let (base, alloc_meta) = {
        let amap = allocs().lock().unwrap();
        match find_alloc_containing(&amap, addr) {
            Some((b, m)) => (b, Some(m.clone())),
            None => (addr, None),
        }
    };

    let mut bmap = borrows().lock().unwrap();
    let stack = match bmap.get_mut(&base) {
        Some(s) => s,
        None => {
            // Fallback for coarse base-key misses: for non-RefMut reads, conservatively
            // check whether another borrow bucket currently holds a unique tag with the
            // same pointee address. This keeps raw/shared conflict detection robust.
            if matches!(access, AliasAccessKind::Read)
                && matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut)
            {
                let tmap = tags().lock().unwrap();
                let sb_pointee = tmap.get(&sb_tag).map(|m| m.pointee_addr).unwrap_or(0);
                let sb_root = sb_lite_root_tag(&tmap, sb_tag);
                // Fallback only on *active* conflicting uniques. Looking at any historical
                // unique entry in other buckets is too noisy and can trigger false positives
                // in large crates where stack buckets are coarse/misaligned.
                let found_conflicting_unique = bmap.values().any(|st| {
                    let Some(e) = st.last() else { return false };
                    if e.kind != BorrowKind::Unique || e.tag == sb_tag {
                        return false;
                    }
                    let same_pointee = tmap
                        .get(&e.tag)
                        .map(|m| m.pointee_addr == sb_pointee)
                        .unwrap_or(false);
                    let same_root = sb_lite_root_tag(&tmap, e.tag) == sb_root;
                    same_pointee && same_root
                });
                if found_conflicting_unique {
                    return Some(format!(
                        "READ via tag={sb_tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top=<missing>",
                        tmeta.kind
                    ));
                }
            }
            return None;
        }
    };

    let top = match stack.last() {
        Some(t) => t.clone(),
        None => return None,
    };

    // If the ref ancestor is alias-exempt (UnsafeCell/interior mutability), skip SB-lite checks.
    if let Some(sb_meta) = tags().lock().unwrap().get(&sb_tag) {
        if sb_meta.alias_exempt {
            return None;
        }
    }

    let dump = if rz_sb_dump_enabled() {
        let tmap = tags().lock().unwrap();
        let mut out = String::new();
        out.push_str("\n-- sb-lite dump --\n");
        out.push_str(&format!("base=0x{base:x} addr=0x{addr:x} size={size}\n"));
        if let Some(ameta) = alloc_meta.as_ref() {
            out.push_str(&format!(
                "alloc: live={} epoch={} size={} is_stack={}\n",
                ameta.live, ameta.epoch, ameta.size, ameta.is_stack
            ));
        } else {
            out.push_str("alloc: <none>\n");
        }
        out.push_str(&format!(
            "tag_meta: orig_tag={} sb_tag={} kind={:?} parent={} pointee=0x{:x} alloc_epoch={} live_at_creation={} escaped={} alias_exempt={}\n",
            orig_tag,
            sb_tag,
            tmeta.kind,
            tmeta.parent,
            tmeta.pointee_addr,
            tmeta.alloc_epoch,
            tmeta.alloc_live_at_creation,
            tmeta.escaped,
            tmeta.alias_exempt
        ));
        out.push_str("tag_ancestry:\n");
        let mut cur = sb_tag;
        for i in 0..32 {
            match tmap.get(&cur) {
                Some(tm) => {
                    out.push_str(&format!(
                        "  {i}: tag={} kind={:?} parent={} pointee=0x{:x} alloc_epoch={} escaped={} alias_exempt={}\n",
                        cur,
                        tm.kind,
                        tm.parent,
                        tm.pointee_addr,
                        tm.alloc_epoch,
                        tm.escaped,
                        tm.alias_exempt
                    ));
                    if tm.parent == 0 {
                        break;
                    }
                    cur = tm.parent;
                }
                None => {
                    out.push_str(&format!("  {i}: tag={} <missing>\n", cur));
                    break;
                }
            }
        }
        out.push_str("borrow_stack (bottom->top):\n");
        for (i, entry) in stack.iter().enumerate() {
            if let Some(tm) = tmap.get(&entry.tag) {
                out.push_str(&format!(
                    "  {i}: tag={} stack_kind={:?} ptr_kind={:?} parent={} pointee=0x{:x} alias_exempt={}\n",
                    entry.tag,
                    entry.kind,
                    tm.kind,
                    tm.parent,
                    tm.pointee_addr,
                    tm.alias_exempt
                ));
            } else {
                out.push_str(&format!(
                    "  {i}: tag={} stack_kind={:?} <missing>\n",
                    entry.tag, entry.kind
                ));
            }
        }
        out.push_str("-- end sb-lite dump --\n");
        out
    } else {
        String::new()
    };

    match access {
        AliasAccessKind::Read => {
            let mut seen_unique = false;
            let mut saw_related = false;
            let (sb_root, sb_pointee) = {
                let tmap = tags().lock().unwrap();
                let root = sb_lite_root_tag(&tmap, sb_tag);
                let pointee = tmap.get(&sb_tag).map(|m| m.pointee_addr).unwrap_or(0);
                (root, pointee)
            };
            for idx in (0..stack.len()).rev() {
                let entry = &stack[idx];
                if entry.tag == sb_tag {
                    saw_related = true;
                    if seen_unique {
                        // Best-effort reactivation for parent unique refs:
                        // if all blockers above are unique descendants of this tag,
                        // treat them as ended and reactivate the parent.
                        if matches!(tmeta.kind, PtrKind::RefMut) {
                            let can_reactivate = {
                                let tmap = tags().lock().unwrap();
                                stack[idx + 1..].iter().all(|e| {
                                    e.kind == BorrowKind::Unique
                                        && sb_lite_tag_is_descendant_of(&tmap, e.tag, sb_tag)
                                })
                            };
                            if can_reactivate {
                                stack.truncate(idx + 1);
                                return None;
                            }
                        }
                        let mut msg = format!(
                            "READ via tag={sb_tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                            tmeta.kind, top.kind, top.tag
                        );
                        msg.push_str(&dump);
                        return Some(msg);
                    }
                    return None;
                }
                if entry.kind == BorrowKind::Unique {
                    let blocker_is_related = {
                        if matches!(tmeta.kind, PtrKind::RefMut) {
                            let tmap = tags().lock().unwrap();
                            let same_root = sb_lite_root_tag(&tmap, entry.tag) == sb_root;
                            let same_pointee = tmap
                                .get(&entry.tag)
                                .map(|m| m.pointee_addr == sb_pointee)
                                .unwrap_or(false);
                            same_root || same_pointee
                        } else {
                            // Keep strict SB-lite behavior for non-RefMut accesses.
                            true
                        }
                    };
                    if blocker_is_related {
                        seen_unique = true;
                        saw_related = true;
                    }
                }
            }
            // No related stack entries means this read belongs to a different borrow lineage.
            // Treat as non-conflicting in SB-lite (best-effort, avoids cross-lineage false positives).
            if !saw_related && matches!(tmeta.kind, PtrKind::RefMut) {
                return None;
            }
            // If the accessing unique tag is missing from stack, but the current stack consists
            // only of unique descendants of that tag, treat this as parent reactivation.
            // This can happen when intermediate reborrows truncated older entries.
            if matches!(tmeta.kind, PtrKind::RefMut) && !stack.is_empty() {
                let can_reactivate_missing_parent = {
                    let tmap = tags().lock().unwrap();
                    stack.iter().all(|e| {
                        e.kind == BorrowKind::Unique
                            && sb_lite_tag_is_descendant_of(&tmap, e.tag, sb_tag)
                    })
                };
                if can_reactivate_missing_parent {
                    stack.clear();
                    stack.push(BorrowEntry {
                        tag: sb_tag,
                        kind: BorrowKind::Unique,
                    });
                    return None;
                }
            }
            let mut msg = format!(
                "READ via tag={sb_tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                tmeta.kind, top.kind, top.tag
            );
            msg.push_str(&dump);
            Some(msg)
        }
        AliasAccessKind::Write => {
            let mut is_top = true;
            let mut seen_unique = false;
            for (idx, entry) in stack.iter().enumerate().rev() {
                if entry.tag == sb_tag {
                    if entry.kind == BorrowKind::Unique {
                        if is_top {
                            return None;
                        }
                        // For raw writes derived from a unique ref, allow shared reborrows
                        // above as a best-effort heuristic (we do not track reborrow ends).
                        if matches!(tmeta.kind, PtrKind::RawMut) && !seen_unique {
                            return None;
                        }
                        // For unique refs, allow reactivation if only shared borrows are above.
                        // Heuristic: if the current top is shared, assume prior unique reborrows
                        // have ended and allow the parent unique to reactivate.
                        if matches!(tmeta.kind, PtrKind::RefMut)
                            && (!seen_unique || matches!(top.kind, BorrowKind::Shared))
                        {
                            stack.truncate(idx + 1);
                            return None;
                        }
                    }
                    let mut msg = format!(
                        "WRITE via tag={sb_tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                        tmeta.kind, top.kind, top.tag
                    );
                    msg.push_str(&dump);
                    return Some(msg);
                }
                if entry.kind == BorrowKind::Unique {
                    seen_unique = true;
                }
                if is_top {
                    is_top = false;
                }
            }
            let mut msg = format!(
                "WRITE via tag={sb_tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                tmeta.kind, top.kind, top.tag
            );
            msg.push_str(&dump);
            Some(msg)
        }
    }
}

fn sb_lite_find_ref_ancestor_tag(tmap: &HashMap<u64, TagMeta>, mut tag: u64) -> Option<u64> {
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
fn sb_lite_tag_is_descendant_of(tmap: &HashMap<u64, TagMeta>, mut tag: u64, ancestor: u64) -> bool {
    if tag == ancestor {
        return true;
    }
    for _ in 0..64 {
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

#[inline]
fn sb_lite_root_tag(tmap: &HashMap<u64, TagMeta>, mut tag: u64) -> u64 {
    if tag == 0 {
        return 0;
    }
    for _ in 0..64 {
        let Some(t) = tmap.get(&tag) else {
            return tag;
        };
        if t.parent == 0 {
            return tag;
        }
        tag = t.parent;
    }
    tag
}
