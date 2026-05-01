use crate::{rz_can_recover_parent_tag, tag_store, PtrKind, TagMeta};
use std::cell::RefCell;

const LINEAGE_CACHE_SLOTS: usize = 512;

#[derive(Copy, Clone)]
struct LineageCacheEntry {
    pointee_addr: usize,
    alloc_epoch: u64,
    any_tag: u64,
    mut_tag: u64,
}

const EMPTY_LINEAGE_CACHE_ENTRY: LineageCacheEntry = LineageCacheEntry {
    pointee_addr: 0,
    alloc_epoch: 0,
    any_tag: 0,
    mut_tag: 0,
};

::std::thread_local! {
    static RZ_LINEAGE_CACHE: RefCell<[LineageCacheEntry; LINEAGE_CACHE_SLOTS]> =
        RefCell::new([EMPTY_LINEAGE_CACHE_ENTRY; LINEAGE_CACHE_SLOTS]);
}

#[inline]
fn lineage_cache_index(pointee_addr: usize, alloc_epoch: u64) -> usize {
    let mut x = pointee_addr as u64;
    x ^= alloc_epoch.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    (x as usize) & (LINEAGE_CACHE_SLOTS - 1)
}

#[inline]
fn is_mut_parent_kind(kind: PtrKind) -> bool {
    matches!(kind, PtrKind::RefMut | PtrKind::RawMut)
}

#[inline]
fn validate_candidate(
    tag: u64,
    pointee_addr: usize,
    alloc_epoch: u64,
    require_mut_parent: bool,
) -> bool {
    if tag == 0 {
        return false;
    }
    let Some(meta) = tag_store::get(tag) else {
        return false;
    };
    if !rz_can_recover_parent_tag(tag) {
        return false;
    }
    if meta.parent == 0 || meta.pointee_addr != pointee_addr {
        return false;
    }
    if meta.alloc_epoch != 0 && meta.alloc_epoch != alloc_epoch {
        return false;
    }
    if require_mut_parent && !is_mut_parent_kind(meta.kind) {
        return false;
    }
    true
}

#[inline]
pub(crate) fn lookup_repaired_parent(
    pointee_addr: usize,
    alloc_epoch: u64,
    require_mut_parent: bool,
) -> Option<u64> {
    if pointee_addr == 0 || alloc_epoch == 0 {
        return None;
    }

    RZ_LINEAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let slot = &mut cache[lineage_cache_index(pointee_addr, alloc_epoch)];
        if slot.pointee_addr != pointee_addr || slot.alloc_epoch != alloc_epoch {
            return None;
        }

        let mut candidates = [0u64; 2];
        if require_mut_parent {
            candidates[0] = slot.mut_tag;
            candidates[1] = slot.any_tag;
        } else {
            candidates[0] = slot.any_tag;
            candidates[1] = slot.mut_tag;
        }

        for tag in candidates {
            if validate_candidate(tag, pointee_addr, alloc_epoch, require_mut_parent) {
                return Some(tag);
            }
        }

        // Stale cache entry.
        slot.any_tag = 0;
        slot.mut_tag = 0;
        None
    })
}

#[inline]
pub(crate) fn remember_non_root_tag(tag: u64, tmeta: &TagMeta) {
    if tag == 0 || tmeta.parent == 0 || tmeta.alloc_epoch == 0 || tmeta.pointee_addr == 0 {
        return;
    }

    RZ_LINEAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let slot = &mut cache[lineage_cache_index(tmeta.pointee_addr, tmeta.alloc_epoch)];
        if slot.pointee_addr != tmeta.pointee_addr || slot.alloc_epoch != tmeta.alloc_epoch {
            *slot = LineageCacheEntry {
                pointee_addr: tmeta.pointee_addr,
                alloc_epoch: tmeta.alloc_epoch,
                any_tag: 0,
                mut_tag: 0,
            };
        }
        slot.any_tag = tag;
        if is_mut_parent_kind(tmeta.kind) {
            slot.mut_tag = tag;
        }
    });
}

#[inline]
pub(crate) fn note_dead_epoch(_base_addr: usize, _alloc_epoch: u64) {
    // The lineage cache is a fixed-size thread-local array keyed by
    // (pointee_addr, alloc_epoch). It does not grow unboundedly, so there is no
    // global dead-epoch state to reclaim here. Dead-epoch entries self-invalidate
    // on the next lookup because tag_store::get() returns compact dead-tag
    // metadata with `parent=0`, which fails candidate validation.
}
