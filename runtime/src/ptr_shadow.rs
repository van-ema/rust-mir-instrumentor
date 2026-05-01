use crate::{lookup_alloc_snapshot, tag_pruning, tag_store};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

const PTR_SLOT_BYTES: usize = core::mem::size_of::<usize>();
const FULL_MASK: u128 = if PTR_SLOT_BYTES >= 128 {
    u128::MAX
} else {
    (1u128 << PTR_SLOT_BYTES) - 1
};

#[derive(Copy, Clone, Debug)]
struct PtrShadowEntry {
    tag: u64,
    ref_ancestor: u64,
    alloc_base: usize,
    alloc_epoch: u64,
}

#[derive(Copy, Clone, Debug)]
struct PartialPtrShadowEntry {
    tag: u64,
    ref_ancestor: u64,
    valid_mask: u128,
    poisoned: bool,
}

static ALLOC_PTR_SHADOW: OnceLock<Mutex<HashMap<(usize, u64), BTreeMap<usize, PtrShadowEntry>>>> =
    OnceLock::new();
static ABS_PTR_SHADOW: OnceLock<Mutex<BTreeMap<usize, PtrShadowEntry>>> = OnceLock::new();
static ALLOC_PTR_SHADOW_PARTIAL: OnceLock<
    Mutex<HashMap<(usize, u64), BTreeMap<usize, PartialPtrShadowEntry>>>,
> = OnceLock::new();
static ABS_PTR_SHADOW_PARTIAL: OnceLock<Mutex<BTreeMap<usize, PartialPtrShadowEntry>>> =
    OnceLock::new();

#[inline]
fn alloc_ptr_shadow() -> &'static Mutex<HashMap<(usize, u64), BTreeMap<usize, PtrShadowEntry>>> {
    ALLOC_PTR_SHADOW.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn abs_ptr_shadow() -> &'static Mutex<BTreeMap<usize, PtrShadowEntry>> {
    ABS_PTR_SHADOW.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[inline]
fn alloc_ptr_shadow_partial(
) -> &'static Mutex<HashMap<(usize, u64), BTreeMap<usize, PartialPtrShadowEntry>>> {
    ALLOC_PTR_SHADOW_PARTIAL.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn abs_ptr_shadow_partial() -> &'static Mutex<BTreeMap<usize, PartialPtrShadowEntry>> {
    ABS_PTR_SHADOW_PARTIAL.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Copy, Clone, Debug)]
enum SlotLoc {
    Alloc {
        base: usize,
        epoch: u64,
        offset: usize,
    },
    Abs {
        addr: usize,
    },
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
fn overlapping_partial_offsets(
    slots: &BTreeMap<usize, PartialPtrShadowEntry>,
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
fn kill_partial_byte_in_map(slots: &mut BTreeMap<usize, PartialPtrShadowEntry>, byte_addr: usize) {
    let scan_start = byte_addr.saturating_sub(PTR_SLOT_BYTES.saturating_sub(1));
    let keys: Vec<usize> = slots
        .range(scan_start..=byte_addr)
        .filter_map(|(&slot_start, _)| {
            let slot_end = slot_start.saturating_add(PTR_SLOT_BYTES);
            if byte_addr < slot_end {
                Some(slot_start)
            } else {
                None
            }
        })
        .collect();

    for slot_start in keys {
        let mut remove = false;
        if let Some(entry) = slots.get_mut(&slot_start) {
            let bit_idx = byte_addr.saturating_sub(slot_start);
            if bit_idx < PTR_SLOT_BYTES {
                entry.valid_mask &= !(1u128 << bit_idx);
            }
            if entry.valid_mask == 0 {
                remove = true;
            }
        }
        if remove {
            slots.remove(&slot_start);
        }
    }
}

#[inline]
fn covering_entry(
    slots: &BTreeMap<usize, PtrShadowEntry>,
    addr: usize,
) -> Option<(usize, PtrShadowEntry, usize)> {
    if PTR_SLOT_BYTES == 0 {
        return None;
    }
    let scan_start = addr.saturating_sub(PTR_SLOT_BYTES.saturating_sub(1));
    slots
        .range(scan_start..=addr)
        .rev()
        .find_map(|(&slot_start, entry)| {
            let slot_end = slot_start.saturating_add(PTR_SLOT_BYTES);
            if addr < slot_end {
                Some((slot_start, *entry, addr.saturating_sub(slot_start)))
            } else {
                None
            }
        })
}

#[inline]
pub(crate) fn kill_range(addr: usize, size: usize) {
    if addr == 0 || size == 0 {
        return;
    }

    match slot_loc(addr) {
        SlotLoc::Alloc {
            base,
            epoch,
            offset,
        } => {
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
            let abs_addr = base.saturating_add(offset);
            let mut abs_shadow = abs_ptr_shadow().lock().unwrap();
            let doomed = overlapping_offsets(&abs_shadow, abs_addr, size);
            for key in doomed {
                abs_shadow.remove(&key);
            }
            let mut partial = alloc_ptr_shadow_partial().lock().unwrap();
            if let Some(slots) = partial.get_mut(&(base, epoch)) {
                let doomed = overlapping_partial_offsets(slots, offset, size);
                for key in doomed {
                    slots.remove(&key);
                }
                if slots.is_empty() {
                    partial.remove(&(base, epoch));
                }
            }
        }
        SlotLoc::Abs { addr } => {
            let mut shadow = abs_ptr_shadow().lock().unwrap();
            let doomed = overlapping_offsets(&shadow, addr, size);
            for key in doomed {
                shadow.remove(&key);
            }
            let mut partial = abs_ptr_shadow_partial().lock().unwrap();
            let doomed = overlapping_partial_offsets(&partial, addr, size);
            for key in doomed {
                partial.remove(&key);
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
    for stored_tag in [tag, ref_ancestor] {
        if stored_tag == 0 {
            continue;
        }
        if let Some(tmeta) = tag_store::mark_escaped(stored_tag) {
            tag_pruning::mark_tag_escaped(stored_tag, &tmeta);
        }
    }

    match slot_loc(addr) {
        SlotLoc::Alloc {
            base,
            epoch,
            offset,
        } => {
            let entry = PtrShadowEntry {
                tag,
                ref_ancestor,
                alloc_base: base,
                alloc_epoch: epoch,
            };
            let mut shadow = alloc_ptr_shadow().lock().unwrap();
            shadow
                .entry((base, epoch))
                .or_default()
                .insert(offset, entry);
            abs_ptr_shadow().lock().unwrap().insert(addr, entry);
        }
        SlotLoc::Abs { addr } => {
            let entry = PtrShadowEntry {
                tag,
                ref_ancestor,
                alloc_base: 0,
                alloc_epoch: 0,
            };
            abs_ptr_shadow().lock().unwrap().insert(addr, entry);
        }
    }
}

#[inline]
fn abs_entry_matches(addr: usize, entry: PtrShadowEntry) -> bool {
    if entry.alloc_epoch == 0 {
        return true;
    }
    lookup_alloc_snapshot(addr).is_some_and(|(base, meta)| {
        meta.live
            && base == entry.alloc_base
            && meta.epoch == entry.alloc_epoch
            && addr.saturating_sub(base).saturating_add(PTR_SLOT_BYTES) <= meta.size
    })
}

#[inline]
fn load_entry(addr: usize) -> Option<PtrShadowEntry> {
    if addr == 0 {
        return None;
    }

    match slot_loc(addr) {
        SlotLoc::Alloc {
            base,
            epoch,
            offset,
        } => {
            let primary = alloc_ptr_shadow()
                .lock()
                .unwrap()
                .get(&(base, epoch))
                .and_then(|slots| slots.get(&offset).copied());
            primary.or_else(|| {
                abs_ptr_shadow()
                    .lock()
                    .unwrap()
                    .get(&addr)
                    .copied()
                    .filter(|entry| abs_entry_matches(addr, *entry))
            })
        }
        SlotLoc::Abs { addr } => abs_ptr_shadow()
            .lock()
            .unwrap()
            .get(&addr)
            .copied()
            .filter(|entry| abs_entry_matches(addr, *entry)),
    }
}

#[inline]
fn load_covering_entry(addr: usize) -> Option<(PtrShadowEntry, usize)> {
    if addr == 0 {
        return None;
    }

    match slot_loc(addr) {
        SlotLoc::Alloc {
            base,
            epoch,
            offset,
        } => alloc_ptr_shadow()
            .lock()
            .unwrap()
            .get(&(base, epoch))
            .and_then(|slots| covering_entry(slots, offset))
            .map(|(_slot_start, entry, byte_off)| (entry, byte_off))
            .or_else(|| {
                abs_ptr_shadow()
                    .lock()
                    .unwrap()
                    .pipe_ref(|slots| covering_entry(slots, addr))
                    .filter(|(slot_start, entry, _)| abs_entry_matches(*slot_start, *entry))
                    .map(|(_slot_start, entry, byte_off)| (entry, byte_off))
            }),
        SlotLoc::Abs { addr } => abs_ptr_shadow()
            .lock()
            .unwrap()
            .pipe_ref(|slots| covering_entry(slots, addr))
            .filter(|(slot_start, entry, _)| abs_entry_matches(*slot_start, *entry))
            .map(|(_slot_start, entry, byte_off)| (entry, byte_off)),
    }
}

trait PipeRef: Sized {
    fn pipe_ref<R>(self, f: impl FnOnce(&Self) -> R) -> R {
        f(&self)
    }
}

impl<T> PipeRef for T {}

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
fn store_partial_byte(addr: usize, src_entry: PtrShadowEntry, src_byte_off: usize) {
    if addr == 0 || src_byte_off >= PTR_SLOT_BYTES {
        return;
    }
    let trace = std::env::var("RZ_TRACE_PTR_SHADOW")
        .ok()
        .is_some_and(|v| v != "0" && v.to_ascii_lowercase() != "false");

    match slot_loc(addr) {
        SlotLoc::Alloc {
            base,
            epoch,
            offset,
        } => {
            let Some(slot_start) = offset.checked_sub(src_byte_off) else {
                return;
            };
            if lookup_alloc_snapshot(base.saturating_add(slot_start))
                .is_none_or(|(_, meta)| slot_start.saturating_add(PTR_SLOT_BYTES) > meta.size)
            {
                return;
            }
            let bit = 1u128 << src_byte_off;
            let mut shadow = alloc_ptr_shadow().lock().unwrap();
            let abs_slot_start = base.saturating_add(slot_start);
            let mut partial = abs_ptr_shadow_partial().lock().unwrap();
            let slot = partial
                .entry(abs_slot_start)
                .or_insert(PartialPtrShadowEntry {
                    tag: src_entry.tag,
                    ref_ancestor: src_entry.ref_ancestor,
                    valid_mask: 0,
                    poisoned: false,
                });
            if !slot.poisoned
                && (slot.tag != src_entry.tag || slot.ref_ancestor != src_entry.ref_ancestor)
            {
                slot.poisoned = true;
                slot.valid_mask = 0;
            }
            if slot.poisoned {
                if trace {
                    eprintln!(
                        "[rusteze-runtime][ptr-shadow] partial_abs poisoned slot=0x{:x} byte_off={}",
                        abs_slot_start, src_byte_off
                    );
                }
                return;
            }
            slot.valid_mask |= bit;
            if trace {
                eprintln!(
                    "[rusteze-runtime][ptr-shadow] partial_abs slot=0x{:x} mask=0x{:x}",
                    abs_slot_start, slot.valid_mask
                );
            }
            if slot.valid_mask == FULL_MASK {
                let full = PtrShadowEntry {
                    tag: slot.tag,
                    ref_ancestor: slot.ref_ancestor,
                    alloc_base: base,
                    alloc_epoch: epoch,
                };
                if trace {
                    eprintln!(
                        "[rusteze-runtime][ptr-shadow] promote_alloc base=0x{:x} epoch={} slot_off={} tag={} ref_ancestor={}",
                        base, epoch, slot_start, full.tag, full.ref_ancestor
                    );
                }
                shadow
                    .entry((base, epoch))
                    .or_default()
                    .insert(slot_start, full);
                abs_ptr_shadow()
                    .lock()
                    .unwrap()
                    .insert(abs_slot_start, full);
                partial.remove(&abs_slot_start);
            }
        }
        SlotLoc::Abs { addr } => {
            let Some(slot_start) = addr.checked_sub(src_byte_off) else {
                return;
            };
            let bit = 1u128 << src_byte_off;
            let mut shadow = abs_ptr_shadow().lock().unwrap();
            let mut partial = abs_ptr_shadow_partial().lock().unwrap();
            let slot = partial.entry(slot_start).or_insert(PartialPtrShadowEntry {
                tag: src_entry.tag,
                ref_ancestor: src_entry.ref_ancestor,
                valid_mask: 0,
                poisoned: false,
            });
            if !slot.poisoned
                && (slot.tag != src_entry.tag || slot.ref_ancestor != src_entry.ref_ancestor)
            {
                slot.poisoned = true;
                slot.valid_mask = 0;
            }
            if slot.poisoned {
                if trace {
                    eprintln!(
                        "[rusteze-runtime][ptr-shadow] partial_abs poisoned slot=0x{:x} byte_off={}",
                        slot_start, src_byte_off
                    );
                }
                return;
            }
            slot.valid_mask |= bit;
            if trace {
                eprintln!(
                    "[rusteze-runtime][ptr-shadow] partial_abs slot=0x{:x} mask=0x{:x}",
                    slot_start, slot.valid_mask
                );
            }
            if slot.valid_mask == FULL_MASK {
                let full = PtrShadowEntry {
                    tag: slot.tag,
                    ref_ancestor: slot.ref_ancestor,
                    alloc_base: 0,
                    alloc_epoch: 0,
                };
                if trace {
                    eprintln!(
                        "[rusteze-runtime][ptr-shadow] promote_abs slot=0x{:x} tag={} ref_ancestor={}",
                        slot_start, full.tag, full.ref_ancestor
                    );
                }
                shadow.insert(slot_start, full);
                partial.remove(&slot_start);
            }
        }
    }
}

#[inline]
pub(crate) fn copy_range(dst_addr: usize, src_addr: usize, size: usize) {
    if dst_addr == 0 || src_addr == 0 || size == 0 {
        return;
    }

    let mut copies = Vec::new();
    for i in 0..size {
        let Some(src_byte_addr) = src_addr.checked_add(i) else {
            break;
        };
        let Some(dst_byte_addr) = dst_addr.checked_add(i) else {
            break;
        };
        if let Some((entry, src_byte_off)) = load_covering_entry(src_byte_addr) {
            copies.push((dst_byte_addr, entry, src_byte_off));
        }
    }

    for i in 0..size {
        let Some(dst_byte_addr) = dst_addr.checked_add(i) else {
            break;
        };
        match slot_loc(dst_byte_addr) {
            SlotLoc::Alloc {
                base,
                epoch,
                offset,
            } => {
                let mut shadow = alloc_ptr_shadow().lock().unwrap();
                if let Some(slots) = shadow.get_mut(&(base, epoch)) {
                    let doomed = overlapping_offsets(slots, offset, 1);
                    for key in doomed {
                        slots.remove(&key);
                    }
                    if slots.is_empty() {
                        shadow.remove(&(base, epoch));
                    }
                }
                let mut partial = alloc_ptr_shadow_partial().lock().unwrap();
                if let Some(slots) = partial.get_mut(&(base, epoch)) {
                    kill_partial_byte_in_map(slots, offset);
                    if slots.is_empty() {
                        partial.remove(&(base, epoch));
                    }
                }
                let abs_addr = base.saturating_add(offset);
                let mut abs_partial = abs_ptr_shadow_partial().lock().unwrap();
                kill_partial_byte_in_map(&mut abs_partial, abs_addr);
            }
            SlotLoc::Abs { addr } => {
                let mut shadow = abs_ptr_shadow().lock().unwrap();
                let doomed = overlapping_offsets(&shadow, addr, 1);
                for key in doomed {
                    shadow.remove(&key);
                }
                let mut partial = abs_ptr_shadow_partial().lock().unwrap();
                kill_partial_byte_in_map(&mut partial, addr);
            }
        }
    }

    for (dst_byte_addr, entry, src_byte_off) in copies {
        store_partial_byte(dst_byte_addr, entry, src_byte_off);
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
    alloc_ptr_shadow_partial()
        .lock()
        .unwrap()
        .remove(&(base_addr, alloc_epoch));
    abs_ptr_shadow()
        .lock()
        .unwrap()
        .retain(|_, entry| !(entry.alloc_base == base_addr && entry.alloc_epoch == alloc_epoch));
}
