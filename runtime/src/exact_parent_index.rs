use crate::{tag_store, PtrKind, TagMeta};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const INDEX_SHARD_COUNT: usize = 64;

#[derive(Copy, Clone, Default)]
struct ExactParentEntry {
    any_tag: u64,
    mut_tag: u64,
}

struct ExactParentShard {
    map: Mutex<HashMap<(usize, u64), ExactParentEntry>>,
}

static EXACT_PARENT_SHARDS: OnceLock<Vec<ExactParentShard>> = OnceLock::new();

#[inline]
fn shards() -> &'static [ExactParentShard] {
    EXACT_PARENT_SHARDS
        .get_or_init(|| {
            (0..INDEX_SHARD_COUNT)
                .map(|_| ExactParentShard {
                    map: Mutex::new(HashMap::new()),
                })
                .collect()
        })
        .as_slice()
}

#[inline]
fn shard_index(pointee_addr: usize, alloc_epoch: u64) -> usize {
    let mut x = (pointee_addr as u64) ^ alloc_epoch.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    (x as usize) & (INDEX_SHARD_COUNT - 1)
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
    if meta.parent == 0 || meta.pointee_addr != pointee_addr || meta.alloc_epoch != alloc_epoch {
        return false;
    }
    if require_mut_parent && !is_mut_parent_kind(meta.kind) {
        return false;
    }
    true
}

#[inline]
pub(crate) fn remember_non_root_tag(tag: u64, tmeta: &TagMeta) {
    if tag == 0 || tmeta.parent == 0 || tmeta.alloc_epoch == 0 || tmeta.pointee_addr == 0 {
        return;
    }

    let idx = shard_index(tmeta.pointee_addr, tmeta.alloc_epoch);
    let mut map = shards()[idx].map.lock().unwrap();
    let entry = map
        .entry((tmeta.pointee_addr, tmeta.alloc_epoch))
        .or_insert_with(ExactParentEntry::default);
    entry.any_tag = tag;
    if is_mut_parent_kind(tmeta.kind) {
        entry.mut_tag = tag;
    }
}

#[inline]
pub(crate) fn lookup(
    pointee_addr: usize,
    alloc_epoch: u64,
    require_mut_parent: bool,
) -> Option<u64> {
    if pointee_addr == 0 || alloc_epoch == 0 {
        return None;
    }

    let idx = shard_index(pointee_addr, alloc_epoch);
    let entry = {
        let map = shards()[idx].map.lock().unwrap();
        *map.get(&(pointee_addr, alloc_epoch))?
    };

    let candidates = if require_mut_parent {
        [entry.mut_tag, entry.any_tag]
    } else {
        [entry.any_tag, entry.mut_tag]
    };

    for tag in candidates {
        if validate_candidate(tag, pointee_addr, alloc_epoch, require_mut_parent) {
            return Some(tag);
        }
    }
    None
}

#[inline]
pub(crate) fn len() -> usize {
    shards()
        .iter()
        .map(|shard| shard.map.lock().unwrap().len())
        .sum()
}
