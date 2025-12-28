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
static CALL_ARG_TAGS: OnceLock<Mutex<HashMap<(u64, u64, usize), u64>>> = OnceLock::new();
static RET_TAGS: OnceLock<Mutex<HashMap<(u64, usize), u64>>> = OnceLock::new();

fn allocs() -> &'static Mutex<HashMap<usize, AllocMeta>> {
    ALLOCS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tags() -> &'static Mutex<HashMap<u64, TagMeta>> {
    TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn call_arg_tags() -> &'static Mutex<HashMap<(u64, u64, usize), u64>> {
    CALL_ARG_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ret_tags() -> &'static Mutex<HashMap<(u64, usize), u64>> {
    RET_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline(never)]
fn rz_violation(kind: &str, msg: String) -> ! {
    // Keep the report on stderr so it is visible even if stdout is buffered.
    eprintln!(
        "\n================ RUSTEZE VIOLATION ================\n{kind}\n{msg}\n===================================================\n"
    );
    // Enable backtraces with `RUST_BACKTRACE=1`.
    panic!("rusteze violation: {kind}");
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

    let new_live = live != 0;

    // We treat `epoch` as an allocation-instance counter for a given base address.
    // We must bump it not only on death, but also on reuse (dead -> live), otherwise
    // a later allocation at the same numeric address could "revive" stale pointers.

    // Death transition: live -> dead
    if !new_live && entry.live {
        entry.epoch = entry.epoch.wrapping_add(1);
    }

    // Reuse/birth transition: dead -> live at an address we've seen before.
    // If we already had a nonzero epoch, bump it so this is a fresh instance.
    if new_live && !entry.live {
        if entry.epoch != 0 {
            entry.epoch = entry.epoch.wrapping_add(1);
        }
    }

    // Mark new liveness state.
    entry.live = new_live;

    // First observation: if epoch is still 0 and it's live, initialize epoch to 1.
    if entry.epoch == 0 && entry.live {
        entry.epoch = 1;
    }

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
        rz_violation(
            "UNKNOWN_TAG",
            format!("WRITE unknown tag={tag} addr=0x{addr:x} size={size}"),
        );
    };

    // Best-effort exact-base allocation lookup (will be extended to range lookup).
    let amap = allocs().lock().unwrap();
    if let Some(ameta) = amap.get(&addr) {
        if !ameta.live {
            rz_violation(
                "USE_AFTER_DEAD",
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\nalloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.epoch,
                    tmeta.alloc_epoch,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
            );
        }
        if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
            rz_violation(
                "STALE_POINTER_EPOCH_MISMATCH",
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\nalloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.epoch,
                    tmeta.alloc_epoch,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
            );
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
        rz_violation(
            "UNKNOWN_TAG",
            format!("READ unknown tag={tag} addr=0x{addr:x} size={size}"),
        );
    };

    // Best-effort exact-base allocation lookup (will be extended to range lookup).
    let amap = allocs().lock().unwrap();
    if let Some(ameta) = amap.get(&addr) {
        if !ameta.live {
            rz_violation(
                "USE_AFTER_DEAD",
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\nalloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.epoch,
                    tmeta.alloc_epoch,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
            );
        }
        if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
            rz_violation(
                "STALE_POINTER_EPOCH_MISMATCH",
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\nalloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.epoch,
                    tmeta.alloc_epoch,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
            );
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

/// Push a pointer-argument tag into a runtime side-channel so callees can retag on entry.
#[no_mangle]
pub extern "C" fn __rz_push_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize, tag: u64) {
    call_arg_tags()
        .lock()
        .unwrap()
        .insert((callee_id, arg_index, addr), tag);
}

/// Take (consume) a pushed pointer-argument tag for a callee/arg/address triple.
#[no_mangle]
pub extern "C" fn __rz_take_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize) -> u64 {
    call_arg_tags()
        .lock()
        .unwrap()
        .remove(&(callee_id, arg_index, addr))
        .unwrap_or(0)
}

/// Push a return-tag into a runtime side-channel so the caller can recover it after the call.
#[no_mangle]
pub extern "C" fn __rz_push_ret_tag(callee_id: u64, addr: usize, tag: u64) {
    ret_tags().lock().unwrap().insert((callee_id, addr), tag);
}

/// Take (consume) a pushed return-tag for a callee/return-address pair.
#[no_mangle]
pub extern "C" fn __rz_take_ret_tag(callee_id: u64, addr: usize) -> u64 {
    ret_tags().lock().unwrap().remove(&(callee_id, addr)).unwrap_or(0)
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

    // IMPORTANT: On retagging/reborrows (parent_tag != 0), we must NOT refresh alloc_epoch by
    // consulting the current allocation map, because the same numeric address can be reused by
    // a different stack frame. Derived tags should inherit the snapshot from their parent tag.
    let alloc_epoch = if parent_tag != 0 {
        tags()
            .lock()
            .unwrap()
            .get(&parent_tag)
            .map(|p| p.alloc_epoch)
            .unwrap_or(0)
    } else {
        // Root creation: best-effort snapshot from current allocation state.
        allocs()
            .lock()
            .unwrap()
            .get(&pointee_addr)
            .map(|m| m.epoch)
            .unwrap_or(0)
    };

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

    // IMPORTANT: On retagging/derived pointers (derived_from != 0), do NOT refresh alloc_epoch
    // from the current allocation map. Inherit it from the parent tag to keep the original
    // allocation-instance snapshot and make stack-slot reuse detectable as stale pointers.
    let alloc_epoch = if derived_from != 0 {
        tags()
            .lock()
            .unwrap()
            .get(&derived_from)
            .map(|p| p.alloc_epoch)
            .unwrap_or(0)
    } else {
        allocs()
            .lock()
            .unwrap()
            .get(&pointee_addr)
            .map(|m| m.epoch)
            .unwrap_or(0)
    };

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
