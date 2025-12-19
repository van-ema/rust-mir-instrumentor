#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]

use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};


static NEXT_TAG: AtomicU64 = AtomicU64::new(1);

/// Metadata for a tracked allocation (stack or heap).
#[derive(Clone, Debug)]
pub struct AllocMeta {
    /// Whether the allocation is currently live.
    pub live: bool,
    /// Monotonically increasing epoch to disambiguate address reuse.
    pub epoch: u64,
    /// Optional size in bytes (0 if unknown).
    pub size: usize,
}

/// Kind of pointer/tag we are tracking.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PtrKind {
    RefShared,
    RefMut,
    RawConst,
    RawMut,
}

/// Metadata associated with a borrow tag.
#[derive(Clone, Debug)]
pub struct TagMeta {
    /// Address of the pointee (exposed provenance / numeric address).
    pub pointee_addr: usize,
    /// Pointer kind (shared/mut ref or const/mut raw).
    pub kind: PtrKind,
    /// Parent/derived tag (0 means none/root).
    pub parent: u64,
    /// Allocation epoch observed at creation time (0 if unknown).
    /// TODO: This is a best-effort snapshot used to disambiguate
    /// address reuse (e.g., stack slots or freed heap memory). Currently,
    /// epochs are matched only when the pointer equals the allocation base
    /// address exactly. This should be extended to:
    ///   - map interior pointers to their base allocation
    ///   - use allocation ranges instead of exact address equality
    ///   - reject accesses when tag.alloc_epoch != alloc.epoch
    pub alloc_epoch: u64,
}

static ALLOCS: OnceLock<Mutex<HashMap<usize, AllocMeta>>> = OnceLock::new();
static TAGS: OnceLock<Mutex<HashMap<u64, TagMeta>>> = OnceLock::new();

fn allocs() -> &'static Mutex<HashMap<usize, AllocMeta>> {
    ALLOCS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tags() -> &'static Mutex<HashMap<u64, TagMeta>> {
    TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record (or update) allocation metadata. The key is the base address.
/// This is a building block; stack/heap instrumentation will call this later.
#[no_mangle]
pub extern "C" fn __rz_record_alloc(base_addr: usize, size: usize, live: u8) {
    let mut m = allocs().lock().unwrap();
    let entry = m.entry(base_addr).or_insert(AllocMeta {
        live: false,
        epoch: 0,
        size,
    });

    // If we are transitioning from dead -> live, bump epoch to disambiguate reuse.
    let new_live = live != 0;
    if new_live && !entry.live {
        entry.epoch = entry.epoch.wrapping_add(1);
    }

    entry.live = new_live;
    // Keep the largest known size if size changes.
    if size != 0 {
        entry.size = entry.size.max(size);
    }
}

/// Read-only helper for debugging/testing.
#[no_mangle]
pub extern "C" fn __rz_dump_state() {
    let a = allocs().lock().unwrap();
    let t = tags().lock().unwrap();
    println!("[rusteze-runtime] allocs={} tags={}", a.len(), t.len());
}

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize, u8, u64) -> u64 = $sym;
    };
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(pointee_addr: usize, is_mut: u8, parent_tag: u64) -> u64 {
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RefMut } else { PtrKind::RefShared };

    // Best-effort: if we have an allocation record at exactly this base address, capture its epoch.
    // (A richer allocator model can later map interior pointers to base allocations.)
    let alloc_epoch = allocs()
        .lock()
        .unwrap()
        .get(&pointee_addr)
        .map(|m| m.epoch)
        .unwrap_or(0);

    tags().lock().unwrap().insert(
        tag,
        TagMeta {
            pointee_addr,
            kind,
            parent: parent_tag,
            alloc_epoch,
        },
    );

    let kind_str = match kind {
        PtrKind::RefShared => "shared",
        PtrKind::RefMut => "mut",
        _ => "?",
    };
    println!(
        "__record_ref_creation called: tag={}, parent={}, pointee=0x{:x}, kind={}",
        tag, parent_tag, pointee_addr, kind_str
    );
    tag
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_raw_ptr_creation"]
pub extern "C" fn __record_raw_ptr_creation(pointee_addr: usize, is_mut: u8, derived_from: u64) -> u64 {
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RawMut } else { PtrKind::RawConst };

    let alloc_epoch = allocs()
        .lock()
        .unwrap()
        .get(&pointee_addr)
        .map(|m| m.epoch)
        .unwrap_or(0);

    tags().lock().unwrap().insert(
        tag,
        TagMeta {
            pointee_addr,
            kind,
            parent: derived_from,
            alloc_epoch,
        },
    );

    let kind_str = match kind {
        PtrKind::RawConst => "const",
        PtrKind::RawMut => "mut",
        _ => "?",
    };
    println!(
        "__record_raw_ptr_creation called: tag={}, from={}, pointee=0x{:x}, kind={}",
        tag, derived_from, pointee_addr, kind_str
    );
    tag
}
