use crate::lookup_alloc_snapshot;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

const PTR_SLOT_BYTES: usize = core::mem::size_of::<usize>();

#[derive(Copy, Clone, Debug)]
struct PtrShadowEntry {
    tag: u64,
    ref_ancestor: u64,
}

static ALLOC_PTR_SHADOW: OnceLock<Mutex<HashMap<(usize, u64), BTreeMap<usize, PtrShadowEntry>>>> =
    OnceLock::new();
static ABS_PTR_SHADOW: OnceLock<Mutex<BTreeMap<usize, PtrShadowEntry>>> = OnceLock::new();

#[inline]
fn alloc_ptr_shadow() -> &'static Mutex<HashMap<(usize, u64), BTreeMap<usize, PtrShadowEntry>>> {
    ALLOC_PTR_SHADOW.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn abs_ptr_shadow() -> &'static Mutex<BTreeMap<usize, PtrShadowEntry>> {
    ABS_PTR_SHADOW.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Copy, Clone, Debug)]
enum SlotLoc {
    Alloc { base: usize, epoch: u64, offset: usize },
    Abs { addr: usize },
}

#[inline]
fn slot_loc(addr: usize) -> SlotLoc {
    if let Some((base, meta)) = lookup_alloc_snapshot(addr) {
        if meta.live {
            if let Some(offset) = addr.checked_sub(base) {
                if meta.size == 0 || offset.saturating_add(PTR_SLOT_BYTES) <= meta.size {
                    return SlotLoc::Alloc {
                        base,
                        epoch: meta.epoch,
                        offset,
                    };
                }
            }
        }
    }

    SlotLoc::Abs { addr }
}

#[inline]
fn overlapping_offsets(
    slots: &BTreeMap<usize, PtrShadowEntry>,
    start: usize,
    size: usize,
) -> Vec<usize> {
    if size == 0 {
        return Vec::new();
    }
    let end = start.saturating_add(size);
    let scan_start = start.saturating_sub(PTR_SLOT_BYTES.saturating_sub(1));
    let mut keys = Vec::new();
    for (&slot_start, _) in slots.range(scan_start..end) {
        let slot_end = slot_start.saturating_add(PTR_SLOT_BYTES);
        if slot_end > start {
            keys.push(slot_start);
        }
    }
    keys
}

#[inline]
pub(crate) fn kill_range(addr: usize, size: usize) {
    if addr == 0 || size == 0 {
        return;
    }

    match slot_loc(addr) {
        SlotLoc::Alloc { base, epoch, offset } => {
            let mut shadow = alloc_ptr_shadow().lock().unwrap();
            if let Some(slots) = shadow.get_mut(&(base, epoch)) {
                let doomed = overlapping_offsets(slots, offset, size);
                for key in doomed {
                    slots.remove(&key);
                }
                if slots.is_empty() {
                    shadow.remove(&(base, epoch));
                }
            }
        }
        SlotLoc::Abs { addr } => {
            let mut shadow = abs_ptr_shadow().lock().unwrap();
            let doomed = overlapping_offsets(&shadow, addr, size);
            for key in doomed {
                shadow.remove(&key);
            }
        }
    }
}

#[inline]
pub(crate) fn store_ptr(addr: usize, tag: u64, ref_ancestor: u64) {
    if addr == 0 {
        return;
    }

    kill_range(addr, PTR_SLOT_BYTES);
    if tag == 0 && ref_ancestor == 0 {
        return;
    }

    let entry = PtrShadowEntry { tag, ref_ancestor };
    match slot_loc(addr) {
        SlotLoc::Alloc { base, epoch, offset } => {
            let mut shadow = alloc_ptr_shadow().lock().unwrap();
            shadow
                .entry((base, epoch))
                .or_default()
                .insert(offset, entry);
        }
        SlotLoc::Abs { addr } => {
            abs_ptr_shadow().lock().unwrap().insert(addr, entry);
        }
    }
}

#[inline]
fn load_entry(addr: usize) -> Option<PtrShadowEntry> {
    if addr == 0 {
        return None;
    }

    match slot_loc(addr) {
        SlotLoc::Alloc { base, epoch, offset } => alloc_ptr_shadow()
            .lock()
            .unwrap()
            .get(&(base, epoch))
            .and_then(|slots| slots.get(&offset).copied()),
        SlotLoc::Abs { addr } => abs_ptr_shadow().lock().unwrap().get(&addr).copied(),
    }
}

#[inline]
pub(crate) fn load_tag(addr: usize) -> u64 {
    load_entry(addr).map(|entry| entry.tag).unwrap_or(0)
}

#[inline]
pub(crate) fn load_ref_ancestor(addr: usize) -> u64 {
    load_entry(addr)
        .map(|entry| entry.ref_ancestor)
        .unwrap_or(0)
}

#[inline]
pub(crate) fn copy_slot(dst_addr: usize, src_addr: usize) {
    if dst_addr == 0 || src_addr == 0 {
        return;
    }
    let entry = load_entry(src_addr);
    kill_range(dst_addr, PTR_SLOT_BYTES);
    if let Some(entry) = entry {
        store_ptr(dst_addr, entry.tag, entry.ref_ancestor);
    }
}

#[inline]
pub(crate) fn remove_alloc_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    alloc_ptr_shadow()
        .lock()
        .unwrap()
        .remove(&(base_addr, alloc_epoch));
}
