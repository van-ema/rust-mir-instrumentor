use crate::{tags, TagMeta};
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const TAG_SHARD_COUNT: usize = 64;

struct TagShard {
    map: Mutex<HashMap<u64, TagMeta>>,
    gen: AtomicU64,
}

static TAG_SHARDS: OnceLock<Vec<TagShard>> = OnceLock::new();

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
    meta
}

#[inline]
pub(crate) fn insert(tag: u64, tmeta: TagMeta) {
    tags().lock().unwrap().insert(tag, tmeta);
    let idx = shard_index(tag);
    shards()[idx].map.lock().unwrap().insert(tag, tmeta);
    shards()[idx].gen.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn update(tag: u64, tmeta: TagMeta) {
    insert(tag, tmeta);
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
