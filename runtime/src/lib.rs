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

/// Record/validate a write through a tracked pointer tag.
/// For now this performs only best-effort checks:
///  - tag must exist
///  - if an allocation record exists at exactly `addr`, it must be live
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
pub extern "C" fn __rz_ptr_write(tag: u64, addr: usize, size: usize) {
    let tmap = tags().lock().unwrap();
    let Some(tmeta) = tmap.get(&tag) else {
        println!("[rusteze-runtime] WRITE: unknown tag={} addr=0x{:x} size={}", tag, addr, size);
        return;
    };

    // Best-effort exact-base allocation lookup (will be extended to range lookup).
    let amap = allocs().lock().unwrap();
    if let Some(ameta) = amap.get(&addr) {
        if !ameta.live {
            println!(
                "[rusteze-runtime] WRITE: use-after-dead tag={} addr=0x{:x} alloc_epoch={} tag_epoch={} kind={:?}",
                tag,
                addr,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind
            );
            return;
        }
        if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
            println!(
                "[rusteze-runtime] WRITE: stale-pointer epoch mismatch tag={} addr=0x{:x} alloc_epoch={} tag_epoch={} kind={:?}",
                tag,
                addr,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind
            );
            return;
        }
    }

    println!(
        "[rusteze-runtime] WRITE: ok tag={} addr=0x{:x} size={} kind={:?}",
        tag,
        addr,
        size,
        tmeta.kind
    );
}

/// Record/validate a read through a tracked pointer tag.
/// For now this performs only best-effort checks:
///  - tag must exist
///  - if an allocation record exists at exactly `addr`, it must be live
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
pub extern "C" fn __rz_ptr_read(tag: u64, addr: usize, size: usize) {
    let tmap = tags().lock().unwrap();
    let Some(tmeta) = tmap.get(&tag) else {
        println!("[rusteze-runtime] READ: unknown tag={} addr=0x{:x} size={}", tag, addr, size);
        return;
    };

    // Best-effort exact-base allocation lookup (will be extended to range lookup).
    let amap = allocs().lock().unwrap();
    if let Some(ameta) = amap.get(&addr) {
        if !ameta.live {
            println!(
                "[rusteze-runtime] READ: use-after-dead tag={} addr=0x{:x} alloc_epoch={} tag_epoch={} kind={:?}",
                tag,
                addr,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind
            );
            return;
        }
        if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
            println!(
                "[rusteze-runtime] READ: stale-pointer epoch mismatch tag={} addr=0x{:x} alloc_epoch={} tag_epoch={} kind={:?}",
                tag,
                addr,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind
            );
            return;
        }
    }

    println!(
        "[rusteze-runtime] READ: ok tag={} addr=0x{:x} size={} kind={:?}",
        tag,
        addr,
        size,
        tmeta.kind
    );
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

/// Generic pointer-use event (coarse).
///
/// a pointer value was used/observed, but we did not (yet) classify it as a read or write.
///
/// `addr` is the pointer value (exposed provenance), not an interior offset.
#[no_mangle]
pub extern "C" fn __rz_ptr_use(tag: u64, addr: usize) {
    if tag == 0 {
        println!(
            "[rusteze-runtime] USE: untagged ptr addr=0x{:x} (likely untracked/propagation missing)",
            addr
        );
        return;
    }

    let tmap = tags().lock().unwrap();
    if let Some(tmeta) = tmap.get(&tag) {
        println!(
            "[rusteze-runtime] USE: tag={} addr=0x{:x} kind={:?} alloc_epoch={} parent={}",
            tag,
            addr,
            tmeta.kind,
            tmeta.alloc_epoch,
            tmeta.parent
        );
    } else {
        println!("[rusteze-runtime] USE: unknown tag={} addr=0x{:x}", tag, addr);
    }
}
