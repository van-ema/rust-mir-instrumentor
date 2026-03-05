use crate::TagMeta;
use std::cell::RefCell;

const TAG_CACHE_SLOTS: usize = 256;

const EMPTY_TAG_META: TagMeta = TagMeta {
    pointee_addr: 0,
    kind: crate::PtrKind::RawConst,
    parent: 0,
    escaped: false,
    alloc_epoch: 0,
    alloc_live_at_creation: false,
    alias_exempt: false,
    lineage_hint: 0,
    bounds_len: 0,
};

#[derive(Copy, Clone)]
struct TagLookupCacheEntry {
    gen: u64,
    tag: u64,
    has_tag: bool,
    meta: TagMeta,
}

const EMPTY_TAG_LOOKUP_CACHE_ENTRY: TagLookupCacheEntry = TagLookupCacheEntry {
    gen: 0,
    tag: 0,
    has_tag: false,
    meta: EMPTY_TAG_META,
};

::std::thread_local! {
    static RZ_TAG_LOOKUP_CACHE: RefCell<[TagLookupCacheEntry; TAG_CACHE_SLOTS]> =
        RefCell::new([EMPTY_TAG_LOOKUP_CACHE_ENTRY; TAG_CACHE_SLOTS]);
}

#[inline]
fn tag_cache_index(tag: u64) -> usize {
    let mut x = tag;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    (x as usize) & (TAG_CACHE_SLOTS - 1)
}

#[inline]
fn tag_cache_get(tag: u64, gen: u64) -> Option<Option<TagMeta>> {
    RZ_TAG_LOOKUP_CACHE.with(|cache| {
        let cache = cache.borrow();
        let entry = cache[tag_cache_index(tag)];
        if entry.gen != gen || entry.tag != tag {
            return None;
        }
        if entry.has_tag {
            Some(Some(entry.meta))
        } else {
            Some(None)
        }
    })
}

#[inline]
fn tag_cache_put(tag: u64, gen: u64, found: Option<TagMeta>) {
    RZ_TAG_LOOKUP_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let slot = &mut cache[tag_cache_index(tag)];
        slot.gen = gen;
        slot.tag = tag;
        match found {
            Some(meta) => {
                slot.has_tag = true;
                slot.meta = meta;
            }
            None => {
                slot.has_tag = false;
                slot.meta = EMPTY_TAG_META;
            }
        }
    });
}

#[inline]
pub(crate) fn get_cached<F>(tag: u64, load: F) -> Option<TagMeta>
where
    F: FnOnce() -> Option<TagMeta>,
{
    let gen = crate::tag_store::gen_for_tag(tag);
    if let Some(found) = tag_cache_get(tag, gen) {
        return found;
    }

    let found = load();
    let gen_now = crate::tag_store::gen_for_tag(tag);
    tag_cache_put(tag, gen_now, found);
    found
}
