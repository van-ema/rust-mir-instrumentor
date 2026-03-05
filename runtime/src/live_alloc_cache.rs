use crate::AllocMeta;
use core::sync::atomic::{AtomicU64, Ordering};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

static LIVE_ALLOCS: OnceLock<Mutex<BTreeMap<usize, AllocMeta>>> = OnceLock::new();
static LIVE_ALLOC_LOOKUP_GEN: AtomicU64 = AtomicU64::new(1);

const LIVE_ALLOC_CACHE_SLOTS: usize = 64;
const CACHE_KIND_EMPTY: u8 = 0;
const CACHE_KIND_MISS_EXACT: u8 = 1;
const CACHE_KIND_HIT_RANGE: u8 = 2;
const CACHE_KIND_HIT_EXACT: u8 = 3;

#[derive(Copy, Clone)]
struct LiveAllocCacheEntry {
    gen: u64,
    kind: u8,
    query_addr: usize,
    base: usize,
    end: usize,
    meta: AllocMeta,
}

const EMPTY_ALLOC_META: AllocMeta = AllocMeta {
    live: false,
    epoch: 0,
    size: 0,
    is_stack: false,
};

const EMPTY_LIVE_ALLOC_CACHE_ENTRY: LiveAllocCacheEntry = LiveAllocCacheEntry {
    gen: 0,
    kind: CACHE_KIND_EMPTY,
    query_addr: 0,
    base: 0,
    end: 0,
    meta: EMPTY_ALLOC_META,
};

::std::thread_local! {
    static RZ_LIVE_ALLOC_LOOKUP_CACHE: RefCell<[LiveAllocCacheEntry; LIVE_ALLOC_CACHE_SLOTS]> =
        RefCell::new([EMPTY_LIVE_ALLOC_CACHE_ENTRY; LIVE_ALLOC_CACHE_SLOTS]);
}

#[inline]
fn live_allocs() -> &'static Mutex<BTreeMap<usize, AllocMeta>> {
    LIVE_ALLOCS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[inline]
fn find_live_alloc_containing<'a>(
    live_map: &'a BTreeMap<usize, AllocMeta>,
    addr: usize,
) -> Option<(usize, &'a AllocMeta)> {
    // Live-only variant of range lookup for hot read/write paths.
    let mut best_live: Option<(usize, &'a AllocMeta, usize)> = None;
    let mut best_unknown: Option<(usize, &'a AllocMeta)> = None;

    for (base, meta) in live_map.range(..=addr).rev() {
        debug_assert!(meta.live, "live allocation index contained a dead entry");
        let size = meta.size;
        if size == 0 {
            if *base == addr && best_live.is_none() {
                best_unknown = Some((*base, meta));
            }
            continue;
        }

        let end = match base.checked_add(size) {
            Some(e) => e,
            None => continue,
        };
        if addr >= end {
            continue;
        }

        match best_live {
            None => best_live = Some((*base, meta, end)),
            Some((_b, _m, best_end)) => {
                if end > best_end {
                    best_live = Some((*base, meta, end));
                }
            }
        }
    }

    if let Some((base, meta, _)) = best_live {
        return Some((base, meta));
    }
    best_unknown
}

#[inline]
fn live_alloc_cache_index(addr: usize) -> usize {
    let mut x = addr as u64;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    (x as usize) & (LIVE_ALLOC_CACHE_SLOTS - 1)
}

#[inline]
fn live_alloc_cache_get(addr: usize, gen: u64) -> Option<Option<(usize, AllocMeta)>> {
    RZ_LIVE_ALLOC_LOOKUP_CACHE.with(|cache| {
        let cache = cache.borrow();
        let entry = cache[live_alloc_cache_index(addr)];
        if entry.gen != gen {
            return None;
        }
        match entry.kind {
            CACHE_KIND_MISS_EXACT => {
                if entry.query_addr == addr {
                    Some(None)
                } else {
                    None
                }
            }
            CACHE_KIND_HIT_RANGE => {
                if addr >= entry.base && addr < entry.end {
                    Some(Some((entry.base, entry.meta)))
                } else {
                    None
                }
            }
            CACHE_KIND_HIT_EXACT => {
                if addr == entry.base {
                    Some(Some((entry.base, entry.meta)))
                } else {
                    None
                }
            }
            _ => None,
        }
    })
}

#[inline]
fn live_alloc_cache_put(addr: usize, gen: u64, found: Option<(usize, AllocMeta)>) {
    RZ_LIVE_ALLOC_LOOKUP_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let slot = &mut cache[live_alloc_cache_index(addr)];
        slot.gen = gen;
        match found {
            Some((base, meta)) => {
                slot.base = base;
                slot.query_addr = addr;
                slot.meta = meta;
                if meta.size == 0 {
                    slot.kind = CACHE_KIND_HIT_EXACT;
                    slot.end = base;
                } else {
                    match base.checked_add(meta.size) {
                        Some(end) => {
                            slot.kind = CACHE_KIND_HIT_RANGE;
                            slot.end = end;
                        }
                        None => {
                            slot.kind = CACHE_KIND_HIT_EXACT;
                            slot.end = base;
                        }
                    }
                }
            }
            None => {
                slot.kind = CACHE_KIND_MISS_EXACT;
                slot.query_addr = addr;
                slot.base = 0;
                slot.end = 0;
                slot.meta = EMPTY_ALLOC_META;
            }
        }
    });
}

#[inline]
pub(crate) fn lookup_containing(addr: usize) -> Option<(usize, AllocMeta)> {
    let gen = LIVE_ALLOC_LOOKUP_GEN.load(Ordering::Relaxed);
    if let Some(found) = live_alloc_cache_get(addr, gen) {
        return found;
    }

    let found = {
        let live_map = live_allocs().lock().unwrap();
        find_live_alloc_containing(&live_map, addr).map(|(base, meta)| (base, *meta))
    };

    let gen_now = LIVE_ALLOC_LOOKUP_GEN.load(Ordering::Relaxed);
    live_alloc_cache_put(addr, gen_now, found);
    found
}

#[inline]
pub(crate) fn update_alloc(base_addr: usize, meta: AllocMeta) {
    let mut live_map = live_allocs().lock().unwrap();
    if meta.live {
        live_map.insert(base_addr, meta);
    } else {
        live_map.remove(&base_addr);
    }
    LIVE_ALLOC_LOOKUP_GEN.fetch_add(1, Ordering::Relaxed);
}
