use crate::{tags, TagMeta};
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const TAG_SHARD_COUNT: usize = 64;

#[derive(Copy, Clone)]
struct CompactTagMeta {
    pointee_addr: usize,
    kind: crate::PtrKind,
    alloc_epoch: u64,
    alloc_live_at_creation: bool,
    exposed_provenance_root: bool,
    bounds_len: usize,
    interior_mut_extent_base: usize,
    interior_mut_extent_len: usize,
    align_req: usize,
    origin_known: bool,
    origin_base: usize,
    origin_end: usize,
}

struct TagShard {
    map: Mutex<HashMap<u64, TagMeta>>,
    gen: AtomicU64,
}

static TAG_SHARDS: OnceLock<Vec<TagShard>> = OnceLock::new();
static HISTORICAL_LIVE_TAGS: OnceLock<Mutex<HashMap<u64, CompactTagMeta>>> = OnceLock::new();
static DEAD_TAGS: OnceLock<Mutex<HashMap<u64, CompactTagMeta>>> = OnceLock::new();
static ALLOC_EPOCH_TAGS: OnceLock<Mutex<HashMap<(usize, u64), Vec<u64>>>> = OnceLock::new();
static LOCAL_HOLDER_COUNTS: OnceLock<Mutex<HashMap<u64, u32>>> = OnceLock::new();

#[inline]
fn shards() -> &'static [TagShard] {
    TAG_SHARDS
        .get_or_init(|| {
            (0..TAG_SHARD_COUNT)
                .map(|_| TagShard {
                    map: Mutex::new(HashMap::new()),
                    gen: AtomicU64::new(1),
                })
                .collect()
        })
        .as_slice()
}

#[inline]
fn historical_live_tags() -> &'static Mutex<HashMap<u64, CompactTagMeta>> {
    HISTORICAL_LIVE_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn dead_tags() -> &'static Mutex<HashMap<u64, CompactTagMeta>> {
    DEAD_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn alloc_epoch_tags() -> &'static Mutex<HashMap<(usize, u64), Vec<u64>>> {
    ALLOC_EPOCH_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn local_holder_counts() -> &'static Mutex<HashMap<u64, u32>> {
    LOCAL_HOLDER_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn shard_index(tag: u64) -> usize {
    let mut x = tag;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    (x as usize) & (TAG_SHARD_COUNT - 1)
}

#[inline]
pub(crate) fn gen_for_tag(tag: u64) -> u64 {
    let idx = shard_index(tag);
    shards()[idx].gen.load(Ordering::Relaxed)
}

#[inline]
pub(crate) fn get(tag: u64) -> Option<TagMeta> {
    let idx = shard_index(tag);
    if let Some(meta) = shards()[idx].map.lock().unwrap().get(&tag).copied() {
        return Some(meta);
    }

    // Fallback to authoritative global map and backfill shard.
    let meta = tags().lock().unwrap().get(&tag).copied();
    if let Some(m) = meta {
        let mut smap = shards()[idx].map.lock().unwrap();
        smap.insert(tag, m);
    }
    if let Some(m) = meta {
        return Some(m);
    }

    if let Some(m) = historical_live_tags().lock().unwrap().get(&tag).copied() {
        return Some(TagMeta {
            pointee_addr: m.pointee_addr,
            kind: m.kind,
            parent: 0,
            escaped: false,
            alloc_epoch: m.alloc_epoch,
            alloc_live_at_creation: m.alloc_live_at_creation,
            alias_exempt: false,
            lineage_hint: 0,
            exposed_provenance_root: m.exposed_provenance_root,
            bounds_len: m.bounds_len,
            interior_mut_extent_base: m.interior_mut_extent_base,
            interior_mut_extent_len: m.interior_mut_extent_len,
            align_req: m.align_req,
            origin_known: m.origin_known,
            origin_base: m.origin_base,
            origin_end: m.origin_end,
        });
    }

    dead_tags()
        .lock()
        .unwrap()
        .get(&tag)
        .copied()
        .map(|m| TagMeta {
            pointee_addr: m.pointee_addr,
            kind: m.kind,
            parent: 0,
            escaped: false,
            alloc_epoch: m.alloc_epoch,
            alloc_live_at_creation: m.alloc_live_at_creation,
            alias_exempt: false,
            lineage_hint: 0,
            exposed_provenance_root: m.exposed_provenance_root,
            bounds_len: m.bounds_len,
            interior_mut_extent_base: m.interior_mut_extent_base,
            interior_mut_extent_len: m.interior_mut_extent_len,
            align_req: m.align_req,
            origin_known: m.origin_known,
            origin_base: m.origin_base,
            origin_end: m.origin_end,
        })
}

#[inline]
pub(crate) fn insert(tag: u64, tmeta: TagMeta) {
    tags().lock().unwrap().insert(tag, tmeta);
}

#[inline]
pub(crate) fn update(tag: u64, tmeta: TagMeta) {
    tags().lock().unwrap().insert(tag, tmeta);
    let idx = shard_index(tag);
    shards()[idx].map.lock().unwrap().insert(tag, tmeta);
    shards()[idx].gen.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn retain_local_holder(tag: u64) {
    if tag == 0 {
        return;
    }
    let mut counts = local_holder_counts().lock().unwrap();
    let entry = counts.entry(tag).or_insert(0);
    *entry = entry.saturating_add(1);
}

#[inline]
pub(crate) fn release_local_holder(tag: u64) -> bool {
    if tag == 0 {
        return false;
    }
    let mut counts = local_holder_counts().lock().unwrap();
    let Some(entry) = counts.get_mut(&tag) else {
        return false;
    };
    if *entry > 1 {
        *entry -= 1;
        return false;
    }
    counts.remove(&tag);
    true
}

#[inline]
pub(crate) fn active_tag_escaped(tag: u64) -> bool {
    tags()
        .lock()
        .unwrap()
        .get(&tag)
        .map(|meta| meta.escaped)
        .unwrap_or(true)
}

#[inline]
pub(crate) fn active_tag_has_local_holder(tag: u64) -> bool {
    if tag == 0 {
        return false;
    }
    local_holder_counts()
        .lock()
        .unwrap()
        .get(&tag)
        .copied()
        .unwrap_or(0)
        != 0
}

#[inline]
pub(crate) fn mark_escaped(tag: u64) -> Option<TagMeta> {
    let updated = {
        let mut tmap = tags().lock().unwrap();
        let tmeta = tmap.get_mut(&tag)?;
        tmeta.escaped = true;
        *tmeta
    };

    let idx = shard_index(tag);
    shards()[idx].map.lock().unwrap().insert(tag, updated);
    shards()[idx].gen.fetch_add(1, Ordering::Relaxed);
    Some(updated)
}

#[inline]
pub(crate) fn len() -> usize {
    tags().lock().unwrap().len()
}

#[inline]
pub(crate) fn dead_len() -> usize {
    dead_tags().lock().unwrap().len()
}

#[inline]
pub(crate) fn historical_live_len() -> usize {
    historical_live_tags().lock().unwrap().len()
}

#[inline]
pub(crate) fn remember_alloc_epoch_tag(base_addr: usize, alloc_epoch: u64, tag: u64) {
    if base_addr == 0 || alloc_epoch == 0 || tag == 0 {
        return;
    }
    let mut idx = alloc_epoch_tags().lock().unwrap();
    idx.entry((base_addr, alloc_epoch)).or_default().push(tag);
}

#[inline]
pub(crate) fn compact_alloc_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    let Some(tags_for_epoch) = alloc_epoch_tags()
        .lock()
        .unwrap()
        .remove(&(base_addr, alloc_epoch))
    else {
        return;
    };

    let mut active = tags().lock().unwrap();
    let mut historical_live = historical_live_tags().lock().unwrap();
    let mut dead = dead_tags().lock().unwrap();
    for tag in tags_for_epoch {
        if let Some(meta) = active.remove(&tag) {
            dead.insert(
                tag,
                CompactTagMeta {
                    pointee_addr: meta.pointee_addr,
                    kind: meta.kind,
                    alloc_epoch: meta.alloc_epoch,
                    alloc_live_at_creation: meta.alloc_live_at_creation,
                    exposed_provenance_root: meta.exposed_provenance_root,
                    bounds_len: meta.bounds_len,
                    interior_mut_extent_base: meta.interior_mut_extent_base,
                    interior_mut_extent_len: meta.interior_mut_extent_len,
                    align_req: meta.align_req,
                    origin_known: meta.origin_known,
                    origin_base: meta.origin_base,
                    origin_end: meta.origin_end,
                },
            );
            let idx = shard_index(tag);
            let mut smap = shards()[idx].map.lock().unwrap();
            if smap.remove(&tag).is_some() {
                shards()[idx].gen.fetch_add(1, Ordering::Relaxed);
            }
        } else if let Some(meta) = historical_live.remove(&tag) {
            dead.insert(tag, meta);
        }
    }
}

#[inline]
pub(crate) fn compact_live_tag(tag: u64) -> bool {
    if tag == 0 {
        return false;
    }

    let Some(meta) = tags().lock().unwrap().remove(&tag) else {
        return false;
    };
    historical_live_tags().lock().unwrap().insert(
        tag,
        CompactTagMeta {
            pointee_addr: meta.pointee_addr,
            kind: meta.kind,
            alloc_epoch: meta.alloc_epoch,
            alloc_live_at_creation: meta.alloc_live_at_creation,
            exposed_provenance_root: meta.exposed_provenance_root,
            bounds_len: meta.bounds_len,
            interior_mut_extent_base: meta.interior_mut_extent_base,
            interior_mut_extent_len: meta.interior_mut_extent_len,
            align_req: meta.align_req,
            origin_known: meta.origin_known,
            origin_base: meta.origin_base,
            origin_end: meta.origin_end,
        },
    );

    let idx = shard_index(tag);
    let mut smap = shards()[idx].map.lock().unwrap();
    if smap.remove(&tag).is_some() {
        shards()[idx].gen.fetch_add(1, Ordering::Relaxed);
    }
    true
}
