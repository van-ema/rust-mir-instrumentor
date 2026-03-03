#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]
use core::ptr;
use std::sync::OnceLock;

mod static_image;
use static_image::StaticRange;
mod alias_model;
use alias_model::{active_alias_model, AliasAccessKind};

::std::thread_local! {
    // Re-entrancy guard to prevent infinite recursion when the runtime allocates
    // while recording allocation metadata.
    static RZ_IN_ALLOC_HOOK: ::std::cell::Cell<bool> = ::std::cell::Cell::new(false);

    // Guard to disable allocator recording while inside any runtime hook.
    // Logging (println!/format!) can allocate while locks are held.
    static RZ_IN_RUNTIME_HOOK: ::std::cell::Cell<u32> = ::std::cell::Cell::new(0);

    // Temporarily suppress SB-lite enforcement for coarse "unknown call" hooks.
    static RZ_SB_SUPPRESS: ::std::cell::Cell<bool> = ::std::cell::Cell::new(false);

    // Temporarily relax epoch-mismatch checks for coarse allow-untagged hooks.
    static RZ_RELAX_EPOCH_CHECK: ::std::cell::Cell<u32> = ::std::cell::Cell::new(0);
}

struct RzRuntimeGuard;
impl RzRuntimeGuard {
    #[inline]
    fn enter() -> Self {
        RZ_IN_RUNTIME_HOOK.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}
impl Drop for RzRuntimeGuard {
    #[inline]
    fn drop(&mut self) {
        RZ_IN_RUNTIME_HOOK.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

struct SbSuppressGuard {
    prev: bool,
}
impl SbSuppressGuard {
    #[inline]
    fn enter() -> Self {
        let prev = RZ_SB_SUPPRESS.with(|c| {
            let p = c.get();
            c.set(true);
            p
        });
        Self { prev }
    }
}
impl Drop for SbSuppressGuard {
    #[inline]
    fn drop(&mut self) {
        RZ_SB_SUPPRESS.with(|c| c.set(self.prev));
    }
}

struct RelaxEpochGuard;
impl RelaxEpochGuard {
    #[inline]
    fn enter() -> Self {
        RZ_RELAX_EPOCH_CHECK.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}
impl Drop for RelaxEpochGuard {
    #[inline]
    fn drop(&mut self) {
        RZ_RELAX_EPOCH_CHECK.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

#[inline]
fn rz_in_runtime_hook() -> bool {
    RZ_IN_RUNTIME_HOOK.with(|c| c.get() != 0)
}

#[inline]
fn rz_sb_suppressed() -> bool {
    RZ_SB_SUPPRESS.with(|c| c.get())
}

#[inline]
fn rz_epoch_check_relaxed() -> bool {
    RZ_RELAX_EPOCH_CHECK.with(|c| c.get() != 0)
}

#[inline]
fn rz_abort_on_double_free() -> bool {
    // Default: abort on double free to avoid cascading UB/noise.
    std::env::var("RZ_ABORT_ON_DOUBLE_FREE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_abort_on_violation() -> bool {
    std::env::var("RZ_ABORT_ON_VIOLATION")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_stack_addr_hint(addr: usize) -> bool {
    // Heuristic: treat addresses within +/-8MiB of the current stack pointer as stack.
    let local = 0u8;
    let sp = &local as *const u8 as usize;
    let lo = sp.saturating_sub(8 * 1024 * 1024);
    let hi = sp.saturating_add(8 * 1024 * 1024);
    addr >= lo && addr <= hi
}

#[inline]
fn rz_tls_addr_hint(addr: usize) -> bool {
    // Heuristic: treat addresses near our thread-local guard as TLS.
    // This suppresses WILD_POINTER reports for thread-local data (e.g., Tokio budget)
    // that is not tracked by stack/heap alloc metadata.
    RZ_IN_RUNTIME_HOOK.with(|c| {
        let tls = c as *const _ as usize;
        let lo = tls.saturating_sub(8 * 1024 * 1024);
        let hi = tls.saturating_add(8 * 1024 * 1024);
        addr >= lo && addr <= hi
    })
}

#[derive(Copy, Clone, Debug)]
enum UntrackedRegionKind {
    Tls,
}

#[inline]
fn rz_untracked_region_strict() -> bool {
    std::env::var("RZ_STRICT_UNTRACKED_REGION")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_tls_pseudo_range() -> (usize, usize) {
    RZ_IN_RUNTIME_HOOK.with(|c| {
        let tls = c as *const _ as usize;
        (
            tls.saturating_sub(8 * 1024 * 1024),
            tls.saturating_add(8 * 1024 * 1024),
        )
    })
}

#[inline]
fn rz_untracked_region_for_access(addr: usize, size: usize) -> Option<UntrackedRegionKind> {
    let (start, end) = rz_tls_pseudo_range();
    let access_end = addr.saturating_add(size.max(1));
    let stack_like = rz_stack_addr_hint(addr)
        || rz_stack_addr_hint(access_end.saturating_sub(1));
    if addr >= start && access_end <= end && !stack_like {
        return Some(UntrackedRegionKind::Tls);
    }
    None
}

#[inline]
fn rz_handle_untracked_region(
    access_kind: &str,
    tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> bool {
    let Some(region) = rz_untracked_region_for_access(addr, size) else {
        return false;
    };

    // Keep TLS suppression narrow: only for tags with unknown allocation provenance
    // that also look TLS-originated. This avoids masking real heap/stack OOB accesses
    // that happen to land inside the coarse TLS pseudo-window.
    if tmeta.alloc_epoch != 0
        || !(rz_tls_addr_hint(tmeta.pointee_addr) || rz_tls_addr_hint(addr))
    {
        return false;
    }

    if rz_untracked_region_strict() {
        let region_name = match region {
            UntrackedRegionKind::Tls => "TLS",
        };
        let msg = append_location_if_enabled(
            format!(
                "{access_kind} via tag={tag} addr=0x{addr:x} size={size}\n(untracked region: {region_name}) kind={:?} parent={} pointee=0x{:x}",
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        rz_violation("UNTRACKED_REGION_ACCESS", msg);
    }
    true
}

fn rz_static_ranges() -> &'static Vec<StaticRange> {
    static RANGES: OnceLock<Vec<StaticRange>> = OnceLock::new();
    RANGES.get_or_init(static_image::collect_static_ranges)
}

#[inline]
fn rz_static_range_for_addr(addr: usize) -> Option<&'static StaticRange> {
    rz_static_ranges()
        .iter()
        .find(|r| addr >= r.start && addr < r.end)
}


#[inline]
fn rz_record_heap_event(ptr: *mut u8, size: usize, live: bool) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        // live=true => alloc, live=false => free
        __rz_record_alloc(ptr as usize, size, if live { 1 } else { 0 });
    }
}

struct RzGlobalAlloc;

unsafe impl ::std::alloc::GlobalAlloc for RzGlobalAlloc {
    unsafe fn alloc(&self, layout: ::std::alloc::Layout) -> *mut u8 {
        if rz_in_runtime_hook() || RZ_IN_ALLOC_HOOK.with(|f| f.get()) {
            return ::std::alloc::System.alloc(layout);
        }
        RZ_IN_ALLOC_HOOK.with(|f| f.set(true));
        let p = ::std::alloc::System.alloc(layout);
        rz_record_heap_event(p, layout.size(), true);
        RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
        p
    }

    unsafe fn alloc_zeroed(&self, layout: ::std::alloc::Layout) -> *mut u8 {
        if rz_in_runtime_hook() || RZ_IN_ALLOC_HOOK.with(|f| f.get()) {
            return ::std::alloc::System.alloc_zeroed(layout);
        }
        RZ_IN_ALLOC_HOOK.with(|f| f.set(true));
        let p = ::std::alloc::System.alloc_zeroed(layout);
        rz_record_heap_event(p, layout.size(), true);
        RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: ::std::alloc::Layout) {
        if rz_in_runtime_hook() || RZ_IN_ALLOC_HOOK.with(|f| f.get()) {
            return ::std::alloc::System.dealloc(ptr, layout);
        }
        RZ_IN_ALLOC_HOOK.with(|f| f.set(true));

        // Validate before calling the system allocator to avoid abort on double-free.
        let ok = rz_pre_free_check(ptr);
        if ok {
            ::std::alloc::System.dealloc(ptr, layout);
        }
        RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: ::std::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        if rz_in_runtime_hook() || RZ_IN_ALLOC_HOOK.with(|f| f.get()) {
            return ::std::alloc::System.realloc(ptr, layout, new_size);
        }
        RZ_IN_ALLOC_HOOK.with(|f| f.set(true));

        // Validate old pointer without marking it dead yet.
        // If invalid/double-freed, skip the system realloc to avoid abort.
        if !ptr.is_null() && !rz_pre_realloc_check(ptr) {
            RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
            return core::ptr::null_mut();
        }

        let p = ::std::alloc::System.realloc(ptr, layout, new_size);
        if p.is_null() {
            // realloc failed (or new_size == 0). If size==0, treat as free.
            if new_size == 0 && !ptr.is_null() {
                __rz_record_alloc(ptr as usize, 0, 0);
            }
            RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
            return p;
        }

        if ptr.is_null() {
            // Null old pointer: realloc behaves like alloc.
            rz_record_heap_event(p, new_size, true);
        } else if p == ptr {
            // Same-base realloc: keep epoch, update size.
            __rz_record_alloc(p as usize, new_size, 1);
        } else {
            // Moved realloc: old base dies, new base is live.
            __rz_record_alloc(ptr as usize, 0, 0);
            __rz_record_alloc(p as usize, new_size, 1);
        }

        RZ_IN_ALLOC_HOOK.with(|f| f.set(false));
        p
    }
}

// Install allocator wrapper globally for any binary linking `runtime`.
#[global_allocator]
static RZ_ALLOC: RzGlobalAlloc = RzGlobalAlloc;

// === end global allocator wrapper ===========================================
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

#[cfg(feature = "rz_log")]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum LogLevel {
    Warn,
    Info,
    Trace,
}

#[cfg(not(feature = "rz_log"))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum LogLevel {
    Warn,
    Info,
    Trace,
}

#[cfg(feature = "rz_log")]
fn rz_log_level() -> LogLevel {
    match std::env::var("RZ_LOG")
        .unwrap_or_else(|_| "warn".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LogLevel::Trace,
        "info" => LogLevel::Info,
        _ => LogLevel::Warn,
    }
}

#[cfg(feature = "rz_log")]
#[inline]
fn rz_log_enabled(level: LogLevel) -> bool {
    rz_log_level() >= level
}

#[cfg(not(feature = "rz_log"))]
#[inline(always)]
fn rz_log_enabled(_level: LogLevel) -> bool {
    false
}

#[cfg(unix)]
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

#[cfg(feature = "rz_log")]
struct RzStackBuf {
    buf: [u8; 1024],
    len: usize,
}

#[cfg(feature = "rz_log")]
impl RzStackBuf {
    #[inline]
    fn new() -> Self {
        Self { buf: [0u8; 1024], len: 0 }
    }

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

#[cfg(feature = "rz_log")]
impl core::fmt::Write for RzStackBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let cap = self.buf.len().saturating_sub(self.len);
        let n = core::cmp::min(cap, bytes.len());
        if n == 0 {
            return Ok(());
        }
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        Ok(())
    }
}

#[cfg(feature = "rz_log")]
#[inline]
fn rz_emit_args(args: core::fmt::Arguments<'_>) {
    #[cfg(unix)]
    unsafe {
        let mut sb = RzStackBuf::new();
        let _ = core::fmt::write(&mut sb, args);
        let b = sb.as_bytes();
        let _ = write(2, b.as_ptr(), b.len());
        let _ = write(2, b"\n".as_ptr(), 1);
    }

    #[cfg(not(unix))]
    {
        eprintln!("{}", args);
    }
}

#[inline]
fn rz_emit_str(s: &str) {
    #[cfg(unix)]
    unsafe {
        let _ = write(2, s.as_bytes().as_ptr(), s.as_bytes().len());
    }

    #[cfg(not(unix))]
    {
        eprint!("{}", s);
    }
}


#[cfg(feature = "rz_log")]
macro_rules! rz_log {
    ($lvl:expr, $($arg:tt)*) => {{
        if rz_log_enabled($lvl) {
            rz_emit_args(format_args!($($arg)*));
        }
    }};
}

#[cfg(not(feature = "rz_log"))]
macro_rules! rz_log {
    ($lvl:expr, $($arg:tt)*) => {{
        let _ = $lvl;
    }};
}

#[cfg(feature = "rz_log")]
macro_rules! rz_warn {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Warn, $($arg)*)
    };
}

#[cfg(not(feature = "rz_log"))]
macro_rules! rz_warn {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Warn, $($arg)*)
    };
}

#[cfg(feature = "rz_log")]
macro_rules! rz_info {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Info, $($arg)*)
    };
}

#[cfg(not(feature = "rz_log"))]
macro_rules! rz_info {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Info, $($arg)*)
    };
}

#[cfg(feature = "rz_log")]
macro_rules! rz_trace {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Trace, $($arg)*)
    };
}

#[cfg(not(feature = "rz_log"))]
macro_rules! rz_trace {
    ($($arg:tt)*) => {
        rz_log!(LogLevel::Trace, $($arg)*)
    };
}


/// Pre-free validation to avoid process abort on double-free/invalid-free.
/// Returns `true` if it is safe to call the underlying system deallocator.
///
/// IMPORTANT: this intentionally diverges from program behavior to keep the
/// process alive long enough to report the violation.
#[inline]
fn rz_pre_free_check(ptr: *mut u8) -> bool {
    if ptr.is_null() {
        return false;
    }

    // Avoid recursion/allocations while inside allocator hooks.
    let _g = RzRuntimeGuard::enter();

    let base = ptr as usize;
    let mut amap = allocs().lock().unwrap();

    match amap.get_mut(&base) {
        None => {
            // This pointer base was not tracked in our allocation map.
            // This can legitimately happen for allocations performed while inside runtime hooks
            // (we intentionally suppress allocator recording to avoid recursion).
            //
            // Default: allow the system deallocator to run to avoid false positives and leaks.
            // Opt-in strict mode: report and skip the system deallocator.
            let strict = std::env::var("RZ_STRICT_FREE_CHECK")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if strict {
                rz_violation(
                    "INVALID_FREE",
                    format!(
                        "FREE of unknown base=0x{base:x} (skipping system dealloc to avoid abort)"
                    ),
                );
                false
            } else {
                rz_trace!(
                    "[rusteze-runtime] note: FREE of untracked base=0x{:x} (allowing system dealloc; set RZ_STRICT_FREE_CHECK=1 for violation)",
                    base
                );
                true
            }
        }
        Some(meta) => {
            if !meta.live {
                rz_violation(
                    "DOUBLE_FREE",
                    format!(
                        "DOUBLE_FREE base=0x{base:x} alloc_epoch={} size={}",
                        meta.epoch, meta.size
                    ),
                );

                if rz_abort_on_double_free() {
                    ::std::process::abort();
                }

                return false;
            }

            // Mark as dead in our bookkeeping now (and bump epoch on death transition).
            // Use the same logic as the normal record path to keep epochs consistent.
            drop(amap);
            __rz_record_alloc(base, 0, 0);
            true
        }
    }
}

/// Pre-realloc validation to avoid process abort on double-free/invalid-free.
/// Returns `true` if it is safe to call the underlying system realloc.
///
/// This check does not update allocation liveness; the caller handles live/dead
/// transitions based on whether the realloc moved the base pointer.
#[inline]
fn rz_pre_realloc_check(ptr: *mut u8) -> bool {
    if ptr.is_null() {
        return true;
    }

    let _g = RzRuntimeGuard::enter();
    let base = ptr as usize;
    let amap = allocs().lock().unwrap();

    match amap.get(&base) {
        None => {
            let strict = std::env::var("RZ_STRICT_FREE_CHECK")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if strict {
                rz_violation(
                    "INVALID_FREE",
                    format!(
                        "REALLOC of unknown base=0x{base:x} (skipping system realloc to avoid abort)"
                    ),
                );
                false
            } else {
                rz_trace!(
                    "[rusteze-runtime] note: REALLOC of untracked base=0x{:x} (allowing system realloc; set RZ_STRICT_FREE_CHECK=1 for violation)",
                    base
                );
                true
            }
        }
        Some(meta) => {
            if !meta.live {
                rz_violation(
                    "DOUBLE_FREE",
                    format!(
                        "DOUBLE_FREE base=0x{base:x} alloc_epoch={} size={}",
                        meta.epoch, meta.size
                    ),
                );

                if rz_abort_on_double_free() {
                    ::std::process::abort();
                }

                return false;
            }
            true
        }
    }
}


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
    /// Whether this allocation came from stack tracking.
    pub is_stack: bool,
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
    /// Whether the pointer has escaped its original scope (e.g., passed across a call boundary).
    pub escaped: bool,
    /// Allocation epoch observed at creation time (0 if unknown).
    /// TODO: This is a best-effort snapshot used to disambiguate
    /// address reuse (e.g., stack slots or freed heap memory). Currently,
    /// epochs are matched only when the pointer equals the allocation base
    /// address exactly. This should be extended to:
    ///   - map interior pointers to their base allocation
    ///   - use allocation ranges instead of exact address equality
    ///   - reject accesses when tag.alloc_epoch != alloc.epoch
    pub alloc_epoch: u64,
    /// Whether the tag was created while the containing allocation was live.
    pub alloc_live_at_creation: bool,
    /// Skip aliasing checks for tags pointing into UnsafeCell / interior mutability.
    pub alias_exempt: bool,
    /// Lineage-repair/suppression hints emitted by instrumentation (bitfield without bit0).
    /// bit1=repair hint, bit2=strong repair/suppression hint, bit3=carry wide bounds from source.
    pub lineage_hint: u8,
    /// Optional bounds length in bytes for wide pointers (slice/str metadata).
    /// 0 means unknown / not provided.
    pub bounds_len: usize,
}

static ALLOCS: OnceLock<Mutex<BTreeMap<usize, AllocMeta>>> = OnceLock::new();
static TAGS: OnceLock<Mutex<HashMap<u64, TagMeta>>> = OnceLock::new();
static CALL_ARG_TAGS: OnceLock<Mutex<HashMap<(u64, u64, usize), u64>>> = OnceLock::new();
static RET_TAGS: OnceLock<Mutex<HashMap<(u64, usize), u64>>> = OnceLock::new();

fn allocs() -> &'static Mutex<BTreeMap<usize, AllocMeta>> {
    ALLOCS.get_or_init(|| Mutex::new(BTreeMap::new()))
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

/// Find the allocation whose range [base, base+size) contains `addr`.
/// Returns (base, meta) if found.
#[inline]
fn find_alloc_containing<'a>(
    amap: &'a BTreeMap<usize, AllocMeta>,
    addr: usize,
) -> Option<(usize, &'a AllocMeta)> {
    // Allocations are half-open ranges: [base, base+size).
    // Prefer a LIVE containing allocation. If none exist, fall back to DEAD,
    // and only then fall back to unknown-size (exact-base) matches.

    let mut best_live: Option<(usize, &'a AllocMeta, usize)> = None; // (base, meta, end)
    let mut best_dead: Option<(usize, &'a AllocMeta, usize)> = None;
    let mut best_unknown: Option<(usize, &'a AllocMeta)> = None;

    for (base, meta) in amap.range(..=addr).rev() {
        let size = meta.size;

        if size == 0 {
            // Unknown-size allocations: only treat as containing if addr == base.
            // Keep as fallback only if we never find a known-size containing allocation.
            if *base == addr && best_live.is_none() && best_dead.is_none() {
                best_unknown = Some((*base, meta));
            }
            continue;
        }

        let end = match base.checked_add(size) {
            Some(e) => e,
            None => continue,
        };

        if addr < end {
            let slot = if meta.live { &mut best_live } else { &mut best_dead };
            match slot {
                None => *slot = Some((*base, meta, end)),
                Some((_b, _m, best_end)) => {
                    if end > *best_end {
                        *slot = Some((*base, meta, end));
                    }
                }
            }
        }
    }

    if let Some((b, m, _end)) = best_live {
        return Some((b, m));
    }
    if let Some((b, m, _end)) = best_dead {
        return Some((b, m));
    }
    best_unknown
}

/// Best-effort lineage repair for stack roots:
/// when instrumentation emits a root raw tag (`parent=0`) for an address that already has
/// same-address non-root tags in the same allocation epoch, attach to the newest one.
/// With a strong hint, also allow bounded overlap-based recovery within the same stack alloc.
#[inline]
fn recover_parent_for_stack_root(
    pointee_addr: usize,
    alloc_epoch: u64,
    is_stack: bool,
    requested_bounds_len: usize,
    strong_hint: bool,
    require_mut_parent: bool,
) -> u64 {
    if !is_stack || alloc_epoch == 0 || pointee_addr == 0 {
        return 0;
    }

    let requested_span = if requested_bounds_len == 0 {
        std::mem::size_of::<usize>()
    } else {
        requested_bounds_len
    };
    let requested_end = pointee_addr.saturating_add(requested_span);

    let amap = allocs().lock().unwrap();
    let root_base = find_alloc_containing(&amap, pointee_addr).map(|(base, _)| base);
    let tmap = tags().lock().unwrap();
    let mut exact_parent = 0u64;
    let mut overlap_parent: Option<(usize, u64)> = None;

    for (&tag, meta) in tmap.iter() {
        if meta.parent == 0 {
            continue;
        }
        if meta.alloc_epoch != 0 && meta.alloc_epoch != alloc_epoch {
            continue;
        }
        if require_mut_parent && !matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut) {
            continue;
        }
        if let Some(base) = root_base {
            let Some((cand_base, _)) = find_alloc_containing(&amap, meta.pointee_addr) else {
                continue;
            };
            if cand_base != base {
                continue;
            }
        }

        if meta.pointee_addr == pointee_addr {
            if tag > exact_parent {
                exact_parent = tag;
            }
            continue;
        }

        if !strong_hint {
            continue;
        }

        let cand_span = if meta.bounds_len == 0 {
            std::mem::size_of::<usize>()
        } else {
            meta.bounds_len
        };
        let cand_end = meta.pointee_addr.saturating_add(cand_span);
        let overlaps = pointee_addr < cand_end && meta.pointee_addr < requested_end;
        if !overlaps {
            continue;
        }

        let distance = if meta.pointee_addr >= pointee_addr {
            meta.pointee_addr - pointee_addr
        } else {
            pointee_addr - meta.pointee_addr
        };
        match overlap_parent {
            None => overlap_parent = Some((distance, tag)),
            Some((best_distance, best_tag)) => {
                if distance < best_distance || (distance == best_distance && tag > best_tag) {
                    overlap_parent = Some((distance, tag));
                }
            }
        }
    }

    if exact_parent != 0 {
        exact_parent
    } else {
        overlap_parent.map(|(_, tag)| tag).unwrap_or(0)
    }
}

#[inline]
fn rz_allow_untracked_stack_ref(tmeta: &TagMeta, addr: usize) -> bool {
    matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
        && tmeta.alloc_epoch == 0
        && (rz_stack_addr_hint(addr)
            || rz_stack_addr_hint(tmeta.pointee_addr)
            || rz_tls_addr_hint(addr)
            || rz_tls_addr_hint(tmeta.pointee_addr))
}

#[inline]
fn rz_allow_untracked_stack_raw_root(tmeta: &TagMeta, addr: usize) -> bool {
    if tmeta.alloc_epoch != 0 {
        return false;
    }
    let is_stack = rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr);
    let is_tls = rz_tls_addr_hint(addr) || rz_tls_addr_hint(tmeta.pointee_addr);
    if !(is_stack || is_tls) {
        return false;
    }

    if !matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        return false;
    }

    // Root untracked stack/TLS raws are low-confidence by construction.
    if tmeta.parent == 0 {
        return true;
    }

    // Optimized MIR often threads pointer values through short derived chains
    // (`Ref -> Raw -> Raw`) while stack-slot metadata is absent. Keep this
    // suppression narrow: only for untracked stack/TLS chains and shallow depth.
    let tmap = tags().lock().unwrap();
    let mut cur = tmeta.parent;
    let mut depth = 0usize;
    while cur != 0 && depth < 4 {
        let Some(pm) = tmap.get(&cur) else {
            break;
        };
        if pm.alloc_epoch == 0
            && (rz_stack_addr_hint(pm.pointee_addr) || rz_tls_addr_hint(pm.pointee_addr))
            && matches!(
                pm.kind,
                PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
            )
        {
            return true;
        }
        cur = pm.parent;
        depth += 1;
    }
    false
}

#[inline]
fn rz_allow_bounded_stack_ref_no_alloc_noise(tmeta: &TagMeta, addr: usize, size: usize) -> bool {
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return false;
    }
    if tmeta.bounds_len == 0 || size == 0 {
        return false;
    }
    if !(rz_stack_addr_hint(addr)
        || rz_stack_addr_hint(tmeta.pointee_addr)
        || rz_tls_addr_hint(addr)
        || rz_tls_addr_hint(tmeta.pointee_addr))
    {
        return false;
    }

    let access_end = match addr.checked_add(size) {
        Some(end) => end,
        None => return false,
    };
    let bounds_end = tmeta.pointee_addr.saturating_add(tmeta.bounds_len);

    // Prefer explicit ref bounds over coarse/missing stack alloc metadata. This catches
    // optimized tail-buffer patterns like `tmpbuf: [u8; 64]` -> `&tmpbuf[..]` -> SIMD lane
    // reads, where the reference metadata is precise but stack-slot tracking only retained a
    // tiny carrier alloc or no alloc at all for later lanes.
    addr >= tmeta.pointee_addr && access_end <= bounds_end
}

#[inline]
fn rz_allow_stack_raw_root_epoch_noise(tmeta: &TagMeta, ameta: &AllocMeta, addr: usize) -> bool {
    // Best-effort suppression for optimized-stack churn:
    // a raw-root const tag (`parent=0`) can survive while stack slots get recycled/re-tagged,
    // yielding epoch mismatches that are not actionable aliasing bugs.
    //
    // Keep this narrow on purpose:
    // - `RawConst` only (do not relax mutating raw flows),
    // - stack-like current address + stack-like tag pointee.
    matches!(tmeta.kind, PtrKind::RawConst)
        && tmeta.parent == 0
        && (ameta.is_stack || rz_stack_addr_hint(addr))
        && rz_stack_addr_hint(tmeta.pointee_addr)
}

#[inline]
fn rz_stack_ref_oob_noise_enabled() -> bool {
    std::env::var("RZ_STACK_REF_OOB_NOISE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_allow_stack_ref_oob_noise(
    tmeta: &TagMeta,
    ameta: &AllocMeta,
    base: usize,
    addr: usize,
    size: usize,
) -> bool {
    if !rz_stack_ref_oob_noise_enabled() {
        return false;
    }
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return false;
    }
    if !(ameta.is_stack || rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr)) {
        return false;
    }

    // Optimized MIR stack-lifetime imprecision can leave overlapping/coarse stack alloc metadata.
    // If the selected containing alloc is clearly inconsistent with the reference metadata, treat
    // this as best-effort tracking noise instead of hard OOB.
    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    let pointee_outside_alloc = tmeta.pointee_addr < base || tmeta.pointee_addr >= alloc_end;
    let access_larger_than_slot = size > ameta.size;
    // Interior references into stack-allocated aggregates can legitimately read/write a value
    // that straddles a coarse tracked slot boundary when optimized MIR loses precise object
    // boundaries for the selected alloc record.
    let crosses_coarse_slot_end = tmeta.parent != 0
        && tmeta.pointee_addr >= base
        && tmeta.pointee_addr < alloc_end
        && access_end > alloc_end;
    pointee_outside_alloc || access_larger_than_slot || crosses_coarse_slot_end
}

#[inline]
fn rz_allow_stack_ref_root_boundary_oob_noise(
    tmeta: &TagMeta,
    ameta: &AllocMeta,
    base: usize,
    addr: usize,
    size: usize,
) -> bool {
    // Narrow fallback for root refs created after lineage loss: a valid field read/write can
    // start at the end of a tiny stack carrier slot (typically pointer-sized pair).
    // On AArch64/NEON this also shows up as 16-byte vector lane reads from a 64-byte temporary
    // (e.g. simd-json stage1's local tmpbuf), where coarse stack-slot tracking records only the
    // first 16-byte lane as the containing alloc.
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return false;
    }
    if tmeta.parent != 0 || tmeta.bounds_len != 0 {
        return false;
    }
    if !(ameta.is_stack || rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr)) {
        return false;
    }

    let usize_sz = std::mem::size_of::<usize>();
    if ameta.size > 2 * usize_sz {
        return false;
    }

    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    let starts_at_boundary = addr >= alloc_end && addr.saturating_sub(alloc_end) <= usize_sz;
    let vector_lane = 2 * usize_sz;
    let small_access = size > 0 && size <= vector_lane;
    starts_at_boundary && small_access && access_end > alloc_end
}

#[inline]
fn rz_allow_stack_raw_root_oob_noise(
    tmeta: &TagMeta,
    ameta: &AllocMeta,
    base: usize,
    addr: usize,
    size: usize,
) -> bool {
    if !matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        return false;
    }
    if tmeta.parent != 0 {
        return false;
    }
    if !(ameta.is_stack || rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr)) {
        return false;
    }

    // Root-tagged stack raws are low-confidence when lineage is missing:
    // coarse stack-slot selection can make boundary and interior-slot reads/writes look OOB.
    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    let near_boundary = addr >= alloc_end && addr.saturating_sub(alloc_end) <= 16;
    let interior_crosses_coarse_slot_end = tmeta.pointee_addr > base
        && tmeta.pointee_addr < alloc_end
        && access_end > alloc_end;
    (near_boundary && size <= 16 && access_end > alloc_end)
        || interior_crosses_coarse_slot_end
}

#[inline]
fn rz_allow_projected_raw_stack_slot_oob_noise(
    tmeta: &TagMeta,
    ameta: &AllocMeta,
    base: usize,
    addr: usize,
    size: usize,
) -> bool {
    if !matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        return false;
    }
    // Strong projected-source hint only; keep this path narrowly targeted.
    if (tmeta.lineage_hint & 0b0000_0100) == 0 || tmeta.parent == 0 {
        return false;
    }
    if !(ameta.is_stack || rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr)) {
        return false;
    }

    let usize_sz = std::mem::size_of::<usize>();
    if ameta.size > 2 * usize_sz || size <= 8 * usize_sz {
        return false;
    }

    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    if access_end <= alloc_end {
        return false;
    }
    if tmeta.pointee_addr < base || tmeta.pointee_addr >= alloc_end {
        return false;
    }

    let tmap = tags().lock().unwrap();
    let mut cur = tmeta.parent;
    let mut depth = 0usize;
    while cur != 0 && depth < 8 {
        let Some(parent) = tmap.get(&cur) else {
            break;
        };
        if parent.pointee_addr >= base && parent.pointee_addr < alloc_end {
            return true;
        }
        cur = parent.parent;
        depth += 1;
    }
    false
}

#[inline(never)]
fn rz_violation(kind: &str, msg: String) {
    // Always print the report. Avoid stdio re-entrancy by writing directly to fd=2.
    rz_emit_str("\n================ RUSTEZE VIOLATION ================\n");
    rz_emit_str(kind);
    rz_emit_str("\n");
    if rz_dump_alloc_on_violation() {
        alloc_log_dump();
    }
    let msg = append_backtrace_if_enabled(msg, "RZ_BACKTRACE");
    rz_emit_str(&msg);
    if !msg.ends_with('\n') {
        rz_emit_str("\n");
    }
    rz_emit_str("===================================================\n\n");

    if rz_abort_on_violation() {
        std::process::abort();
    }

    // Fail-fast only if requested
    let failfast = std::env::var("RUSTEZE_FAILFAST").ok().map_or(false, |v| v != "0");
    if failfast {
        // Enable backtraces with `RUST_BACKTRACE=1`
        panic!("rusteze violation: {kind}");
    }
}

fn backtrace_enabled(var: &str) -> bool {
    std::env::var(var)
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[cfg(feature = "rz_alloc_dump")]
#[inline]
fn rz_dump_alloc_on_violation() -> bool {
    std::env::var("RZ_DUMP_ALLOC_ON_VIOLATION")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[cfg(not(feature = "rz_alloc_dump"))]
#[inline]
fn rz_dump_alloc_on_violation() -> bool {
    false
}

#[cfg(feature = "rz_alloc_dump")]
#[inline]
fn rz_dump_alloc_match_addr_enabled() -> bool {
    std::env::var("RZ_DUMP_ALLOC_MATCH_ADDR")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[cfg(not(feature = "rz_alloc_dump"))]
#[inline]
fn rz_dump_alloc_match_addr_enabled() -> bool {
    false
}

#[inline]
fn rz_allow_untagged() -> bool {
    std::env::var("RZ_ALLOW_UNTAGGED")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_tag0_as_root() -> bool {
    std::env::var("RZ_TAG0_AS_ROOT")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_log_alloc_enabled() -> bool {
    std::env::var("RZ_LOG_ALLOC")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[cfg(feature = "rz_log")]
#[inline]
fn rz_emit_alloc(args: core::fmt::Arguments<'_>) {
    rz_emit_args(args);
}

#[cfg(not(feature = "rz_log"))]
#[inline]
fn rz_emit_alloc(_args: core::fmt::Arguments<'_>) {}

fn append_backtrace_if_enabled(mut msg: String, var: &str) -> String {
    if backtrace_enabled(var) {
        let bt = std::backtrace::Backtrace::force_capture();
        msg.push_str("\nbacktrace:\n");
        msg.push_str(&format!("{bt:?}"));
    }
    msg
}

fn location_enabled(var: &str) -> bool {
    std::env::var(var)
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[track_caller]
fn append_location_if_enabled(mut msg: String, var: &str) -> String {
    if location_enabled(var) {
        let loc = std::panic::Location::caller();
        msg.push_str(&format!(
            "\nloc={}:{}:{}",
            loc.file(),
            loc.line(),
            loc.column()
        ));
    }
    msg
}

/// Best-effort: resolve the allocation that a tag is derived from.
///
/// We walk up the tag-parent chain and try to map a tag's `pointee_addr` to an allocation
/// using range-based lookup. We only accept the allocation if its epoch matches the tag's
/// recorded `alloc_epoch` (when both are nonzero). This prevents misclassifying manually
/// crafted pointers as OOB relative to an unrelated nearby allocation.
#[inline]
fn origin_alloc_for_tag<'a>(
    tmap: &HashMap<u64, TagMeta>,
    amap: &'a BTreeMap<usize, AllocMeta>,
    mut tag: u64,
) -> Option<(usize, &'a AllocMeta)> {
    // Limit parent-walk to avoid pathological cycles.
    for _ in 0..32 {
        let t: &TagMeta = tmap.get(&tag)?;

        if let Some((base, ameta)) = find_alloc_containing(amap, t.pointee_addr) {
            // If both sides have epochs, require a match.
            if t.alloc_epoch != 0 && ameta.epoch != 0 && t.alloc_epoch != ameta.epoch {
                // Epoch mismatch: treat as unrelated (likely address reuse / stale).
            } else {
                return Some((base, ameta));
            }
        }

        if t.parent == 0 {
            break;
        }
        tag = t.parent;
    }
    None
}

/// Record (or update) allocation metadata. The key is the base address.
/// This is a building block; stack/heap instrumentation will call this later.
#[no_mangle]
pub extern "C" fn __rz_record_alloc(base_addr: usize, size: usize, live: u8) {
    let _g = RzRuntimeGuard::enter();

    // Record allocation events into a fixed-size ring buffer for post-mortem dumps.
    alloc_log_record(base_addr, size, live);

    if rz_log_alloc_enabled() {
        // Emit allocation events regardless of RZ_LOG level.
        let new_live = (live & 0x1) != 0;
        let is_stack = (live & 0x2) != 0;
        rz_emit_alloc(format_args!(
            "[rusteze-runtime] record_alloc base=0x{:x} size={} live={} is_stack={}",
            base_addr,
            size,
            new_live,
            is_stack
        ));
    } else if rz_log_enabled(LogLevel::Trace) {
        // `live` bit 0: live/dead. bit 1: stack marker.
        let new_live = (live & 0x1) != 0;
        let is_stack = (live & 0x2) != 0;
        rz_trace!(
            "[rusteze-runtime] record_alloc base=0x{:x} size={} live={} is_stack={}",
            base_addr,
            size,
            new_live,
            is_stack
        );
    }

    let mut m = allocs().lock().unwrap();
    let is_stack = (live & 0x2) != 0;
    let entry = m.entry(base_addr).or_insert(AllocMeta {
        live: false,
        epoch: 0,
        size,
        is_stack,
    });

    if is_stack {
        entry.is_stack = true;
    }

    let new_live = (live & 0x1) != 0;

    // We treat `epoch` as an allocation-instance counter for a given base address.
    // We must bump it not only on death, but also on reuse (dead -> live), otherwise
    // a later allocation at the same numeric address could "revive" stale pointers.

    // Death transition: live to dead
    if !new_live && entry.live {
        entry.epoch = entry.epoch.wrapping_add(1);
    }

    // Reuse/birth transition: dead to live at an address we've seen before.
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

    drop(m);
    active_alias_model().on_alloc_state_change(base_addr, new_live);
}

// === allocation event ring buffer (no-alloc, best-effort) ===================

#[cfg(feature = "rz_alloc_dump")]
const ALLOC_LOG_SIZE: usize = 4096;
#[cfg(feature = "rz_alloc_dump")]
const ALLOC_LOG_HEAP_SIZE: usize = 4096;

#[cfg(feature = "rz_alloc_dump")]
#[derive(Copy, Clone)]
struct AllocLogEntry {
    seq: u64,
    base: usize,
    size: usize,
    live: u8,
}

#[cfg(feature = "rz_alloc_dump")]
impl AllocLogEntry {
    const fn empty() -> Self {
        Self { seq: 0, base: 0, size: 0, live: 0 }
    }
}

#[cfg(feature = "rz_alloc_dump")]
static ALLOC_LOG_IDX: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "rz_alloc_dump")]
static ALLOC_LOG_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "rz_alloc_dump")]
static mut ALLOC_LOG: [AllocLogEntry; ALLOC_LOG_SIZE] = [AllocLogEntry::empty(); ALLOC_LOG_SIZE];
#[cfg(feature = "rz_alloc_dump")]
static ALLOC_LOG_HEAP_IDX: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "rz_alloc_dump")]
static ALLOC_LOG_HEAP_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "rz_alloc_dump")]
static mut ALLOC_LOG_HEAP: [AllocLogEntry; ALLOC_LOG_HEAP_SIZE] =
    [AllocLogEntry::empty(); ALLOC_LOG_HEAP_SIZE];

#[cfg(feature = "rz_alloc_dump")]
#[inline]
fn alloc_log_record(base_addr: usize, size: usize, live: u8) {
    let idx = ALLOC_LOG_IDX.fetch_add(1, Ordering::Relaxed) % ALLOC_LOG_SIZE;
    let seq = ALLOC_LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    unsafe {
        ALLOC_LOG[idx] = AllocLogEntry { seq, base: base_addr, size, live };
    }
    // Also keep a heap-only ring buffer to avoid stack noise.
    if (live & 0x2) == 0 {
        let hidx = ALLOC_LOG_HEAP_IDX.fetch_add(1, Ordering::Relaxed) % ALLOC_LOG_HEAP_SIZE;
        let hseq = ALLOC_LOG_HEAP_SEQ.fetch_add(1, Ordering::Relaxed);
        unsafe {
            ALLOC_LOG_HEAP[hidx] = AllocLogEntry { seq: hseq, base: base_addr, size, live };
        }
    }
}

#[cfg(not(feature = "rz_alloc_dump"))]
#[inline]
fn alloc_log_record(_base_addr: usize, _size: usize, _live: u8) {}

#[cfg(feature = "rz_alloc_dump")]
fn alloc_log_dump() {
    // Write last N entries in reverse order (most recent first).
    struct LocalBuf {
        buf: [u8; 256],
        len: usize,
    }
    impl LocalBuf {
        #[inline]
        fn new() -> Self {
            Self { buf: [0u8; 256], len: 0 }
        }
        #[inline]
        fn as_bytes(&self) -> &[u8] {
            &self.buf[..self.len]
        }
    }
    impl core::fmt::Write for LocalBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let cap = self.buf.len().saturating_sub(self.len);
            let n = core::cmp::min(cap, bytes.len());
            if n == 0 {
                return Ok(());
            }
            self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
            self.len += n;
            Ok(())
        }
    }

    let heap_only = std::env::var("RZ_DUMP_ALLOC_HEAP_ONLY")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");
    if heap_only {
        rz_emit_str("\n[rusteze-runtime] last heap alloc events (most recent first):\n");
    } else {
        rz_emit_str("\n[rusteze-runtime] last alloc events (most recent first):\n");
    }

    let mut seen = 0usize;
    let (head, cap, buf_ptr) = if heap_only {
        (
            ALLOC_LOG_HEAP_IDX.load(Ordering::Relaxed),
            ALLOC_LOG_HEAP_SIZE,
            unsafe { &raw const ALLOC_LOG_HEAP as *const [AllocLogEntry; ALLOC_LOG_HEAP_SIZE] }
        )
    } else {
        (
            ALLOC_LOG_IDX.load(Ordering::Relaxed),
            ALLOC_LOG_SIZE,
            unsafe { &raw const ALLOC_LOG as *const [AllocLogEntry; ALLOC_LOG_SIZE] }
        )
    };

    for i in 0..cap {
        let idx = (head.wrapping_sub(1 + i)) % cap;
        let entry = unsafe { (*buf_ptr)[idx] };
        if entry.seq == 0 {
            continue;
        }
        let new_live = (entry.live & 0x1) != 0;
        let is_stack = (entry.live & 0x2) != 0;
        if heap_only && is_stack {
            continue;
        }
        let mut buf = LocalBuf::new();
        let _ = core::fmt::write(
            &mut buf,
            format_args!(
                "  seq={} base=0x{:x} size={} live={} is_stack={}\n",
                entry.seq, entry.base, entry.size, new_live, is_stack
            ),
        );
        rz_emit_str(core::str::from_utf8(buf.as_bytes()).unwrap_or(""));
        seen += 1;
        if seen >= 256 {
            break;
        }
    }
    if seen == 0 {
        rz_emit_str("  (no entries)\n");
    }
}

#[cfg(not(feature = "rz_alloc_dump"))]
fn alloc_log_dump() {}

#[cfg(feature = "rz_alloc_dump")]
fn alloc_log_dump_contains(addr: usize) {
    struct LocalBuf {
        buf: [u8; 256],
        len: usize,
    }
    impl LocalBuf {
        #[inline]
        fn new() -> Self {
            Self { buf: [0u8; 256], len: 0 }
        }
        #[inline]
        fn as_bytes(&self) -> &[u8] {
            &self.buf[..self.len]
        }
    }
    impl core::fmt::Write for LocalBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let cap = self.buf.len().saturating_sub(self.len);
            let n = core::cmp::min(cap, bytes.len());
            if n == 0 {
                return Ok(());
            }
            self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
            self.len += n;
            Ok(())
        }
    }

    rz_emit_str("\n[rusteze-runtime] heap alloc events containing addr:\n");
    let mut seen = 0usize;
    let head = ALLOC_LOG_HEAP_IDX.load(Ordering::Relaxed);
    for i in 0..ALLOC_LOG_HEAP_SIZE {
        let idx = (head.wrapping_sub(1 + i)) % ALLOC_LOG_HEAP_SIZE;
        let entry = unsafe { ALLOC_LOG_HEAP[idx] };
        if entry.seq == 0 || entry.size == 0 {
            continue;
        }
        let end = match entry.base.checked_add(entry.size) {
            Some(e) => e,
            None => usize::MAX,
        };
        if addr < entry.base || addr >= end {
            continue;
        }
        let new_live = (entry.live & 0x1) != 0;
        let mut buf = LocalBuf::new();
        let _ = core::fmt::write(
            &mut buf,
            format_args!(
                "  seq={} base=0x{:x} end=0x{:x} size={} live={}\n",
                entry.seq, entry.base, end, entry.size, new_live
            ),
        );
        rz_emit_str(core::str::from_utf8(buf.as_bytes()).unwrap_or(""));
        seen += 1;
        if seen >= 32 {
            break;
        }
    }
    if seen == 0 {
        rz_emit_str("  (no matches)\n");
    }
}

#[cfg(not(feature = "rz_alloc_dump"))]
fn alloc_log_dump_contains(_addr: usize) {}

/// Read-only helper for debugging/testing.
#[no_mangle]
pub extern "C" fn __rz_dump_state() {
    let _g = RzRuntimeGuard::enter();
    let a = allocs().lock().unwrap();
    let t = tags().lock().unwrap();
    rz_info!("[rusteze-runtime] allocs={} tags={}", a.len(), t.len());
}

/// Record/validate a write through a tracked pointer tag.
/// For now this performs only best-effort checks:
///  - tag must exist
///  - if an allocation record exists at exactly `addr`, it must be live
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
#[track_caller]
pub fn __rz_ptr_write(tag: u64, addr: usize, size: usize) {
    let tag = if tag == 0 {
        if rz_allow_untagged() {
            return;
        }
        if rz_tag0_as_root() {
            __record_raw_ptr_creation(addr, 1, 0, 0, 0)
        } else {
            tag
        }
    } else {
        tag
    };
    if size == 0 {
        if rz_log_enabled(LogLevel::Trace) {
            rz_trace!(
                "[rusteze-runtime] zero-size WRITE: tag={} addr=0x{:x}",
                tag,
                addr
            );
        }
        return;
    }
    let _g = RzRuntimeGuard::enter();
    let (tmeta, sb_tag_opt) = {
        let tmap = tags().lock().unwrap();
        let Some(tmeta) = tmap.get(&tag) else {
            let msg = append_location_if_enabled(
                format!("WRITE unknown tag={tag} addr=0x{addr:x} size={size}"),
                "RZ_LOG_LOC",
            );
            rz_violation(
                "UNKNOWN_TAG",
                msg,
            );
            return;
        };
        let sb_tag = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
            active_alias_model().find_ref_ancestor_tag(&tmap, tag)
        } else {
            Some(tag)
        };
        (tmeta.clone(), sb_tag)
    };

    if let Some(sb_tag) = sb_tag_opt {
        if let Some(msg) = active_alias_model().check_access(
            sb_tag,
            tag,
            &tmeta,
            addr,
            size,
            AliasAccessKind::Write,
        ) {
            rz_violation(
                active_alias_model().violation_kind(),
                append_location_if_enabled(msg, "RZ_LOG_LOC"),
            );
            return;
        }
    }

    // Range-based allocation lookup.
    let amap = allocs().lock().unwrap();
    let alloc_opt = find_alloc_containing(&amap, addr);
    if rz_log_enabled(LogLevel::Trace) {
        rz_trace!("[rusteze-runtime] WRITE lookup: addr=0x{:x} size={} tag={}", addr, size, tag);
        // Print up to 8 nearest bases <= addr for debugging.
        let mut shown = 0usize;
        for (b, m) in amap.range(..=addr).rev() {
            if shown >= 8 { break; }
            let end = b.saturating_add(m.size);
            rz_trace!("  cand base=0x{:x} size={} live={} epoch={} end=0x{:x}", b, m.size, m.live, m.epoch, end);
            shown += 1;
        }
    }

    let Some((base, ameta)) = alloc_opt else {
        if rz_handle_untracked_region("WRITE", tag, &tmeta, addr, size) {
            return;
        }

        // Best-effort: when stack allocation metadata is missing, do not classify
        // references into the current stack window as wild pointers.
        if rz_allow_untracked_stack_ref(&tmeta, addr)
            || rz_allow_untracked_stack_raw_root(&tmeta, addr)
            || rz_allow_bounded_stack_ref_no_alloc_noise(&tmeta, addr, size)
        {
            return;
        }

        if rz_log_enabled(LogLevel::Trace) {
            rz_trace!("[rusteze-runtime] WRITE lookup result: no containing allocation");
        }
        // If we can prove (via tag provenance + epoch snapshot) that this pointer was derived
        // from a particular allocation, classify this as OUT_OF_BOUNDS rather than WILD_POINTER.
        let tmap = tags().lock().unwrap();
        if let Some((obase, ometa)) = origin_alloc_for_tag(&tmap, &amap, tag) {
            if ometa.size != 0 && size != 0 {
                let access_end = addr.saturating_add(size);
                let alloc_end = obase.saturating_add(ometa.size);

                // If the access overlaps beyond the end of the origin allocation, it's OOB.
                if addr >= obase && access_end > alloc_end {
                    if rz_allow_stack_ref_oob_noise(&tmeta, ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_ref_root_boundary_oob_noise(&tmeta, ometa, obase, addr, size)
                    {
                        return;
                    }
                    if rz_allow_stack_raw_root_oob_noise(&tmeta, ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_projected_raw_stack_slot_oob_noise(
                        &tmeta, ometa, obase, addr, size,
                    ) {
                        return;
                    }
                    let msg = append_location_if_enabled(
                        format!(
                            "WRITE via tag={tag} addr=0x{addr:x} size={size}\n(no containing alloc for addr, but tag derives from alloc)\norigin_alloc_base=0x{obase:x} origin_alloc_end=0x{alloc_end:x} origin_alloc_size={} origin_epoch={} tag_epoch={} kind={:?} parent={} pointee=0x{:x}",
                            ometa.size,
                            ometa.epoch,
                            tmeta.alloc_epoch,
                            tmeta.kind,
                            tmeta.parent,
                            tmeta.pointee_addr
                        ),
                        "RZ_LOG_LOC",
                    );
                    if rz_dump_alloc_match_addr_enabled() {
                        alloc_log_dump_contains(addr);
                    }
                    rz_violation(
                        "OUT_OF_BOUNDS",
                        msg,
                    );
                    return;
                }
            }
        }

        if let Some(r) = rz_static_range_for_addr(addr) {
            if size == 0 || addr.saturating_add(size) <= r.end {
                if r.writable {
                    return;
                }
                let msg = append_location_if_enabled(
                    format!(
                        "WRITE via tag={tag} addr=0x{addr:x} size={size}\n(write to read-only static range 0x{:x}..0x{:x}) kind={:?} parent={} pointee=0x{:x}",
                        r.start,
                        r.end,
                        tmeta.kind,
                        tmeta.parent,
                        tmeta.pointee_addr
                    ),
                    "RZ_LOG_LOC",
                );
                rz_violation(
                    "WRITE_TO_READONLY_STATIC",
                    msg,
                );
                return;
            }
        }

        let msg = append_location_if_enabled(
            format!(
                "WRITE via tag={tag} addr=0x{addr:x} size={size}\n(no allocation contains this address) kind={:?} parent={} pointee=0x{:x}",
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "WILD_POINTER",
            msg,
        );
        return;
    };

    if rz_log_enabled(LogLevel::Trace) {
        let alloc_end = base.saturating_add(ameta.size);
        rz_trace!(
            "[rusteze-runtime] WRITE lookup result: base=0x{:x} size={} live={} epoch={} alloc_end=0x{:x}",
            base,
            ameta.size,
            ameta.live,
            ameta.epoch,
            alloc_end
        );
    }

    if !ameta.live {
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
        {
            return;
        }
        if ameta.is_stack && !tmeta.alloc_live_at_creation {
            return;
        }
        let msg = append_location_if_enabled(
            format!(
                "WRITE via tag={tag} addr=0x{addr:x} size={size}\nalloc_base=0x{base:x} alloc_size={} alloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                ameta.size,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "USE_AFTER_DEAD",
            msg,
        );
        return;
    }

    if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
        // Shared references in safe code frequently get recreated across allocator-address reuse;
        // treating their epoch mismatch as hard UB is too noisy.
        if matches!(tmeta.kind, PtrKind::RefShared) {
            return;
        }
        // Best-effort stack policy: optimized MIR can miss precise stack liveness boundaries,
        // so Ref*/stack epoch mismatches are often frame-reuse noise.
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
        {
            return;
        }
        // Ignore known stack raw-root epoch churn noise (see helper for scope).
        if rz_allow_stack_raw_root_epoch_noise(&tmeta, &ameta, addr) {
            return;
        }
        if rz_epoch_check_relaxed() && (ameta.is_stack || rz_stack_addr_hint(addr)) {
            return;
        }
        let msg = append_location_if_enabled(
            format!(
                "WRITE via tag={tag} addr=0x{addr:x} size={size}\nalloc_base=0x{base:x} alloc_size={} alloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                ameta.size,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "STALE_POINTER_EPOCH_MISMATCH",
            msg,
        );
        return;
    }

    // Bounds check against wide-pointer metadata (slice/str) if available.
    if tmeta.bounds_len != 0 && size != 0 {
        let access_end = match addr.checked_add(size) {
            Some(e) => e,
            None => {
                let msg = append_location_if_enabled(
                    format!(
                        "WRITE via tag={tag} addr=0x{addr:x} size={size}\naddress overflow\nbounds_base=0x{:x} bounds_len={}\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                        tmeta.pointee_addr,
                        tmeta.bounds_len,
                        ameta.size,
                        tmeta.kind,
                        tmeta.parent,
                        tmeta.pointee_addr
                    ),
                    "RZ_LOG_LOC",
                );
                rz_violation(
                    "OUT_OF_BOUNDS",
                    msg,
                );
                return;
            }
        };
        let bounds_end = tmeta.pointee_addr.saturating_add(tmeta.bounds_len);
        if addr < tmeta.pointee_addr || access_end > bounds_end {
            let msg = append_location_if_enabled(
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\naccess_end=0x{access_end:x} bounds_base=0x{:x} bounds_end=0x{bounds_end:x} bounds_len={}\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                    tmeta.pointee_addr,
                    tmeta.bounds_len,
                    ameta.size,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation(
                "OUT_OF_BOUNDS",
                msg,
            );
            return;
        }
    }

    // OOB check if both the access size and allocation size are known.
    if size != 0 && ameta.size != 0 {
        let end = match addr.checked_add(size) {
            Some(e) => e,
            None => {
                let msg = append_location_if_enabled(
                    format!(
                        "WRITE via tag={tag} addr=0x{addr:x} size={size}\naddress overflow\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={} pointee=0x{:x}",
                        ameta.size,
                        tmeta.kind,
                        tmeta.parent,
                        tmeta.pointee_addr
                    ),
                    "RZ_LOG_LOC",
                );
                if rz_dump_alloc_match_addr_enabled() {
                    alloc_log_dump_contains(addr);
                }
                rz_violation(
                    "OUT_OF_BOUNDS",
                    msg,
                );
                return;
            }
        };

        let alloc_end = match base.checked_add(ameta.size) {
            Some(e) => e,
            None => usize::MAX,
        };

        if end > alloc_end {
            if rz_allow_stack_ref_oob_noise(&tmeta, &ameta, base, addr, size)
                || rz_allow_stack_raw_root_oob_noise(&tmeta, &ameta, base, addr, size)
                || rz_allow_projected_raw_stack_slot_oob_noise(&tmeta, &ameta, base, addr, size)
            {
                return;
            }
            let msg = append_location_if_enabled(
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\naccess_end=0x{end:x} alloc_base=0x{base:x} alloc_end=0x{alloc_end:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.size,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            if rz_dump_alloc_match_addr_enabled() {
                alloc_log_dump_contains(addr);
            }
            rz_violation(
                "OUT_OF_BOUNDS",
                msg,
            );
            return;
        }
    }

    rz_info!(
        "[rusteze-runtime] WRITE: ok tag={} addr=0x{:x} size={} kind={:?}",
        tag,
        addr,
        size,
        tmeta.kind
    );
}

/// Like `__rz_ptr_write`, but silently skips untagged pointers (tag=0).
#[no_mangle]
#[track_caller]
pub fn __rz_ptr_write_allow_untagged(tag: u64, addr: usize, size: usize) {
    if tag == 0 {
        return;
    }
    let _sb = SbSuppressGuard::enter();
    let _relax = RelaxEpochGuard::enter();
    __rz_ptr_write(tag, addr, size);
}

/// Record/validate a read through a tracked pointer tag.
/// For now this performs only best-effort checks:
///  - tag must exist
///  - if an allocation record exists at exactly `addr`, it must be live
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
#[track_caller]
pub fn __rz_ptr_read(tag: u64, addr: usize, size: usize) {
    let tag = if tag == 0 {
        if rz_allow_untagged() {
            return;
        }
        if rz_tag0_as_root() {
            __record_raw_ptr_creation(addr, 0, 0, 0, 0)
        } else {
            tag
        }
    } else {
        tag
    };
    if size == 0 {
        if rz_log_enabled(LogLevel::Trace) {
            rz_trace!(
                "[rusteze-runtime] zero-size READ: tag={} addr=0x{:x}",
                tag,
                addr
            );
        }
        return;
    }
    let _g = RzRuntimeGuard::enter();
    let (tmeta, sb_tag_opt) = {
        let tmap = tags().lock().unwrap();
        let Some(tmeta) = tmap.get(&tag) else {
            let msg = append_location_if_enabled(
                format!("READ unknown tag={tag} addr=0x{addr:x} size={size}"),
                "RZ_LOG_LOC",
            );
            rz_violation(
                "UNKNOWN_TAG",
                msg,
            );
            return;
        };
        let sb_tag = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
            active_alias_model().find_ref_ancestor_tag(&tmap, tag)
        } else {
            Some(tag)
        };
        (tmeta.clone(), sb_tag)
    };

    if let Some(sb_tag) = sb_tag_opt {
        if let Some(msg) = active_alias_model().check_access(
            sb_tag,
            tag,
            &tmeta,
            addr,
            size,
            AliasAccessKind::Read,
        ) {
            rz_violation(
                active_alias_model().violation_kind(),
                append_location_if_enabled(msg, "RZ_LOG_LOC"),
            );
            return;
        }
    }

    // Range-based allocation lookup.
    let amap = allocs().lock().unwrap();
    let alloc_opt = find_alloc_containing(&amap, addr);

    let Some((base, ameta)) = alloc_opt else {
        if rz_handle_untracked_region("READ", tag, &tmeta, addr, size) {
            return;
        }

        // Best-effort: when stack allocation metadata is missing, do not classify
        // references into the current stack window as wild pointers.
        if rz_allow_untracked_stack_ref(&tmeta, addr)
            || rz_allow_untracked_stack_raw_root(&tmeta, addr)
            || rz_allow_bounded_stack_ref_no_alloc_noise(&tmeta, addr, size)
        {
            return;
        }

        // If we can prove (via tag provenance + epoch snapshot) that this pointer was derived
        // from a particular allocation, classify this as OUT_OF_BOUNDS rather than WILD_POINTER.
        let tmap = tags().lock().unwrap();
        if let Some((obase, ometa)) = origin_alloc_for_tag(&tmap, &amap, tag) {
            if ometa.size != 0 && size != 0 {
                let access_end = addr.saturating_add(size);
                let alloc_end = obase.saturating_add(ometa.size);

                if addr >= obase && access_end > alloc_end {
                    if rz_allow_stack_ref_oob_noise(&tmeta, ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_ref_root_boundary_oob_noise(&tmeta, ometa, obase, addr, size)
                    {
                        return;
                    }
                    if rz_allow_stack_raw_root_oob_noise(&tmeta, ometa, obase, addr, size) {
                        return;
                    }
                    let msg = append_location_if_enabled(
                        format!(
                            "READ via tag={tag} addr=0x{addr:x} size={size}\n(no containing alloc for addr, but tag derives from alloc)\norigin_alloc_base=0x{obase:x} origin_alloc_end=0x{alloc_end:x} origin_alloc_size={} origin_epoch={} tag_epoch={} kind={:?} parent={} pointee=0x{:x}",
                            ometa.size,
                            ometa.epoch,
                            tmeta.alloc_epoch,
                            tmeta.kind,
                            tmeta.parent,
                            tmeta.pointee_addr
                        ),
                        "RZ_LOG_LOC",
                    );
                    if rz_dump_alloc_match_addr_enabled() {
                        alloc_log_dump_contains(addr);
                    }
                    rz_violation(
                        "OUT_OF_BOUNDS",
                        msg,
                    );
                    return;
                }
            }
        }

        if let Some(r) = rz_static_range_for_addr(addr) {
            if size == 0 || addr.saturating_add(size) <= r.end {
                return;
            }
        }

        let msg = append_location_if_enabled(
            format!(
                "READ via tag={tag} addr=0x{addr:x} size={size}\n(no allocation contains this address) kind={:?} parent={} pointee=0x{:x}",
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "WILD_POINTER",
            msg,
        );
        return;
    };

    if !ameta.live {
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
        {
            return;
        }
        if ameta.is_stack && !tmeta.alloc_live_at_creation {
            return;
        }
        let msg = append_location_if_enabled(
            format!(
                "READ via tag={tag} addr=0x{addr:x} size={size}\nalloc_base=0x{base:x} alloc_size={} alloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                ameta.size,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "USE_AFTER_DEAD",
            msg,
        );
        return;
    }

    if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
        // Shared references in safe code frequently get recreated across allocator-address reuse;
        // treating their epoch mismatch as hard UB is too noisy.
        if matches!(tmeta.kind, PtrKind::RefShared) {
            return;
        }
        // Best-effort stack policy: optimized MIR can miss precise stack liveness boundaries,
        // so Ref*/stack epoch mismatches are often frame-reuse noise.
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
        {
            return;
        }
        // Ignore known stack raw-root epoch churn noise (see helper for scope).
        if rz_allow_stack_raw_root_epoch_noise(&tmeta, &ameta, addr) {
            return;
        }
        if rz_epoch_check_relaxed() && (ameta.is_stack || rz_stack_addr_hint(addr)) {
            return;
        }
        let msg = append_location_if_enabled(
            format!(
                "READ via tag={tag} addr=0x{addr:x} size={size}\nalloc_base=0x{base:x} alloc_size={} alloc_epoch={} tag_epoch={} kind={:?} parent={}\npointee=0x{:x}",
                ameta.size,
                ameta.epoch,
                tmeta.alloc_epoch,
                tmeta.kind,
                tmeta.parent,
                tmeta.pointee_addr
            ),
            "RZ_LOG_LOC",
        );
        if rz_dump_alloc_match_addr_enabled() {
            alloc_log_dump_contains(addr);
        }
        rz_violation(
            "STALE_POINTER_EPOCH_MISMATCH",
            msg,
        );
        return;
    }

    // Bounds check against wide-pointer metadata (slice/str) if available.
    if tmeta.bounds_len != 0 && size != 0 {
        let access_end = match addr.checked_add(size) {
            Some(e) => e,
            None => {
                let msg = append_location_if_enabled(
                    format!(
                        "READ via tag={tag} addr=0x{addr:x} size={size}\naddress overflow\nbounds_base=0x{:x} bounds_len={}\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                        tmeta.pointee_addr,
                        tmeta.bounds_len,
                        ameta.size,
                        tmeta.kind,
                        tmeta.parent,
                        tmeta.pointee_addr
                    ),
                    "RZ_LOG_LOC",
                );
                rz_violation(
                    "OUT_OF_BOUNDS",
                    msg,
                );
                return;
            }
        };
        let bounds_end = tmeta.pointee_addr.saturating_add(tmeta.bounds_len);
        if addr < tmeta.pointee_addr || access_end > bounds_end {
            let msg = append_location_if_enabled(
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\naccess_end=0x{access_end:x} bounds_base=0x{:x} bounds_end=0x{bounds_end:x} bounds_len={}\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                    tmeta.pointee_addr,
                    tmeta.bounds_len,
                    ameta.size,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation(
                "OUT_OF_BOUNDS",
                msg,
            );
            return;
        }
    }

    // OOB check if both the access size and allocation size are known.
    if size != 0 && ameta.size != 0 {
        let end = match addr.checked_add(size) {
            Some(e) => e,
            None => {
                let msg = append_location_if_enabled(
                    format!(
                        "READ via tag={tag} addr=0x{addr:x} size={size}\naddress overflow\nalloc_base=0x{base:x} alloc_size={} kind={:?} parent={} pointee=0x{:x}",
                        ameta.size,
                        tmeta.kind,
                        tmeta.parent,
                        tmeta.pointee_addr
                    ),
                    "RZ_LOG_LOC",
                );
                if rz_dump_alloc_match_addr_enabled() {
                    alloc_log_dump_contains(addr);
                }
                rz_violation(
                    "OUT_OF_BOUNDS",
                    msg,
                );
                return;
            }
        };

        let alloc_end = match base.checked_add(ameta.size) {
            Some(e) => e,
            None => usize::MAX,
        };

        if end > alloc_end {
            if rz_allow_stack_ref_oob_noise(&tmeta, &ameta, base, addr, size)
                || rz_allow_stack_raw_root_oob_noise(&tmeta, &ameta, base, addr, size)
            {
                return;
            }
            let msg = append_location_if_enabled(
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\naccess_end=0x{end:x} alloc_base=0x{base:x} alloc_end=0x{alloc_end:x} alloc_size={} kind={:?} parent={}\npointee=0x{:x}",
                    ameta.size,
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            if rz_dump_alloc_match_addr_enabled() {
                alloc_log_dump_contains(addr);
            }
            rz_violation(
                "OUT_OF_BOUNDS",
                msg,
            );
            return;
        }
    }

    rz_info!(
        "[rusteze-runtime] READ: ok tag={} addr=0x{:x} size={} kind={:?}",
        tag,
        addr,
        size,
        tmeta.kind
    );
}

/// Like `__rz_ptr_read`, but silently skips untagged pointers (tag=0).
#[no_mangle]
#[track_caller]
pub fn __rz_ptr_read_allow_untagged(tag: u64, addr: usize, size: usize) {
    if tag == 0 {
        return;
    }
    let _sb = SbSuppressGuard::enter();
    let _relax = RelaxEpochGuard::enter();
    __rz_ptr_read(tag, addr, size);
}

/// Push a pointer-argument tag into a runtime side-channel so callees can retag on entry.
#[no_mangle]
pub extern "C" fn __rz_push_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize, tag: u64) {
    let _g = RzRuntimeGuard::enter();
    call_arg_tags()
        .lock()
        .unwrap()
        .insert((callee_id, arg_index, addr), tag);
}

/// Take (consume) a pushed pointer-argument tag for a callee/arg/address triple.
#[no_mangle]
pub extern "C" fn __rz_take_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = call_arg_tags()
        .lock()
        .unwrap()
        .remove(&(callee_id, arg_index, addr))
        .unwrap_or(0);
    if tag != 0 {
        active_alias_model().on_call_arg_taken(callee_id, tag);
    }
    tag
}

/// Push a return-tag into a runtime side-channel so the caller can recover it after the call.
#[no_mangle]
pub extern "C" fn __rz_push_ret_tag(callee_id: u64, addr: usize, tag: u64) {
    let _g = RzRuntimeGuard::enter();
    ret_tags().lock().unwrap().insert((callee_id, addr), tag);
}

/// Take (consume) a pushed return-tag for a callee/return-address pair.
#[no_mangle]
pub extern "C" fn __rz_take_ret_tag(callee_id: u64, addr: usize) -> u64 {
    let _g = RzRuntimeGuard::enter();
    ret_tags().lock().unwrap().remove(&(callee_id, addr)).unwrap_or(0)
}

/// Take a pushed return-tag, or fall back to a fresh raw-pointer tag if missing.
///
/// This avoids UNKNOWN_TAG when a callee didn't push a return tag (or the key mismatched),
/// while still preserving inter-procedural tags when available.
#[no_mangle]
pub extern "C" fn __rz_take_ret_tag_or_root(
    callee_id: u64,
    addr: usize,
    is_mut: u8,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = { ret_tags().lock().unwrap().remove(&(callee_id, addr)).unwrap_or(0) };
    if tag != 0 {
        return tag;
    }
    // Fallback: synthesize a fresh raw-pointer tag rooted at this address.
    __record_raw_ptr_creation(addr, is_mut, 0, alias_exempt, bounds_len)
}

/// Notify runtime alias models that the current instrumented function is exiting.
#[no_mangle]
pub extern "C" fn __rz_exit_fn(callee_id: u64) {
    let _g = RzRuntimeGuard::enter();
    active_alias_model().on_call_exit(callee_id);
}

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize, u8, u64, u8, usize) -> u64 = $sym;
    };
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(
    pointee_addr: usize,
    is_mut: u8,
    parent_tag: u64,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RefMut } else { PtrKind::RefShared };
    // `alias_exempt` is a bitfield emitted by instrumentation:
    // - bit0: alias-exempt classification
    // - bit1: basic lineage-repair hint
    // - bit2: strong root-origin repair hint
    let alias_exempt_flag = (alias_exempt & 0b0000_0001) != 0;
    let projected_ref_hint = (alias_exempt & 0b0000_0010) != 0;
    let projected_ref_strong_hint = (alias_exempt & 0b0000_0100) != 0;

    if let Some(msg) = active_alias_model().validate_ref_creation(
        pointee_addr,
        kind,
        parent_tag,
        alias_exempt_flag,
        bounds_len,
    ) {
        rz_violation(
            active_alias_model().violation_kind(),
            append_location_if_enabled(msg, "RZ_LOG_LOC"),
        );
    }

    // IMPORTANT: On retagging/reborrows (parent_tag != 0), prefer inheriting the parent's
    // allocation snapshot to keep stack-slot reuse detectable.
    // Exception: if the new ref clearly points into a different allocation than the parent
    // pointee, refresh to the pointee allocation snapshot (common in projection-heavy code).
    let mut alloc_is_stack = false;
    let mut alloc_size = 0usize;
    let (mut alloc_epoch, mut alloc_live_at_creation, mut inherited_bounds_len, mut resolved_parent_tag) =
        if parent_tag != 0 {
        let (parent_epoch, parent_live, parent_pointee, inherited_bounds_len) = tags()
            .lock()
            .unwrap()
            .get(&parent_tag)
            .map(|p| (p.alloc_epoch, p.alloc_live_at_creation, Some(p.pointee_addr), p.bounds_len))
            .unwrap_or((0, false, None, 0));

        if let Some(parent_pointee) = parent_pointee {
            let amap = allocs().lock().unwrap();
            let parent_alloc = find_alloc_containing(&amap, parent_pointee);
            let pointee_alloc = find_alloc_containing(&amap, pointee_addr);
            match (parent_alloc, pointee_alloc) {
                (Some((parent_base, _parent_meta)), Some((pointee_base, pointee_meta))) => {
                    alloc_is_stack = pointee_meta.is_stack;
                    alloc_size = pointee_meta.size;
                    if parent_base != pointee_base
                        || (parent_epoch != 0
                            && pointee_meta.epoch != 0
                            && parent_epoch != pointee_meta.epoch)
                    {
                        (pointee_meta.epoch, pointee_meta.live, 0, parent_tag)
                    } else {
                        (parent_epoch, parent_live, inherited_bounds_len, parent_tag)
                    }
                }
                (Some((parent_base, parent_meta)), None) => {
                    let parent_end = parent_base.saturating_add(parent_meta.size);
                    // If the new pointee is clearly outside the parent's allocation and we
                    // cannot resolve any allocation for it, lineage is likely crossing objects
                    // (e.g., wrapper/metadata paths). Break ancestry to avoid false OOB/UAF
                    // classification from inherited parent allocation metadata.
                    if parent_meta.size != 0
                        && !(pointee_addr >= parent_base && pointee_addr < parent_end)
                    {
                        (0, false, 0, 0)
                    } else {
                        (parent_epoch, parent_live, inherited_bounds_len, parent_tag)
                    }
                }
                (None, Some((_pointee_base, pointee_meta))) => {
                    alloc_is_stack = pointee_meta.is_stack;
                    alloc_size = pointee_meta.size;
                    (pointee_meta.epoch, pointee_meta.live, 0, parent_tag)
                }
                (None, None) => {
                    // Parent metadata already detached from a concrete allocation.
                    // Continuing to inherit it creates cascading false OOB/UAF reports.
                    if parent_epoch != 0 {
                        (0, false, 0, 0)
                    } else {
                        (parent_epoch, parent_live, inherited_bounds_len, parent_tag)
                    }
                }
            }
        } else {
            (parent_epoch, parent_live, inherited_bounds_len, parent_tag)
        }
    } else {
        // Root creation: snapshot from the allocation that contains this address (range lookup).
        // If the match is a dead stack slot, treat metadata as unknown to avoid
        // inheriting stale bounds/epoch from recycled stack storage.
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, pointee_addr)
            .map(|(_base, m)| {
                alloc_is_stack = m.is_stack;
                alloc_size = m.size;
                if m.is_stack && !m.live {
                    (0, false, 0, 0)
                } else {
                    (m.epoch, m.live, 0, 0)
                }
            })
            .unwrap_or((0, false, 0, 0))
    };

    // Optimized MIR frequently loses parent tags for stack refs at call boundaries and in
    // projection-heavy lowering. Exact same-address recovery is low-risk for refs, so allow it
    // for all stack roots; the stronger bounded-overlap recovery remains gated by the hint bit.
    if resolved_parent_tag == 0
        && alloc_is_stack
        && alloc_epoch != 0
        && alloc_size >= std::mem::size_of::<usize>()
    {
        let requested_bounds = if bounds_len != 0 {
            bounds_len
        } else {
            inherited_bounds_len
        };
        let repaired_parent = recover_parent_for_stack_root(
            pointee_addr,
            alloc_epoch,
            true,
            requested_bounds,
            projected_ref_strong_hint,
            matches!(kind, PtrKind::RefMut),
        );
        if repaired_parent != 0 {
            resolved_parent_tag = repaired_parent;
            if let Some(parent_meta) = tags().lock().unwrap().get(&resolved_parent_tag).cloned() {
                if inherited_bounds_len == 0 {
                    inherited_bounds_len = parent_meta.bounds_len;
                }
                if alloc_epoch == 0 && parent_meta.alloc_epoch != 0 {
                    alloc_epoch = parent_meta.alloc_epoch;
                    alloc_live_at_creation = parent_meta.alloc_live_at_creation;
                }
            }
            rz_trace!(
                "__record_ref_creation lineage repair: pointee=0x{:x} from={} -> {} epoch={} size={}",
                pointee_addr,
                parent_tag,
                resolved_parent_tag,
                alloc_epoch,
                alloc_size
            );
        }
    }

    let bounds_len = if bounds_len != 0 { bounds_len } else { inherited_bounds_len };

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: resolved_parent_tag,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt_flag,
        lineage_hint: alias_exempt & 0b0000_1110,
        bounds_len,
    };
    tags().lock().unwrap().insert(tag, tmeta.clone());
    active_alias_model().on_tag_created(tag, &tmeta);

    let kind_str = match kind {
        PtrKind::RefShared => "shared",
        PtrKind::RefMut => "mut",
        _ => "?",
    };
    rz_trace!(
        "__record_ref_creation called: tag={}, parent={}, pointee=0x{:x}, kind={}",
        tag,
        parent_tag,
        pointee_addr,
        kind_str
    );
    tag
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_raw_ptr_creation"]
pub extern "C" fn __record_raw_ptr_creation(
    pointee_addr: usize,
    is_mut: u8,
    derived_from: u64,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RawMut } else { PtrKind::RawConst };
    // `alias_exempt` is a bitfield emitted by instrumentation:
    // - bit0: alias-exempt classification
    // - bit1: basic lineage-repair hint
    // - bit2: strong root-origin repair hint
    // - bit3: carry wide bounds from the source pointer when metadata is intentionally dropped
    let alias_exempt_flag = (alias_exempt & 0b0000_0001) != 0;
    let projected_raw_hint = (alias_exempt & 0b0000_0010) != 0;
    let projected_raw_strong_hint = (alias_exempt & 0b0000_0100) != 0;
    let carry_bounds_from_source = (alias_exempt & 0b0000_1000) != 0;
    let mut resolved_parent = derived_from;
    let mut alloc_is_stack = false;
    let mut alloc_size = 0usize;
    let mut parent_alloc_mismatch = false;
    let mut parent_is_root = false;
    let mut parent_pointee_addr: Option<usize> = None;

    // IMPORTANT: On retagging/derived pointers (derived_from != 0), prefer inheriting the
    // parent's alloc_epoch to keep the original allocation-instance snapshot and make
    // stack-slot reuse detectable as stale pointers. Exception: if the derived pointer
    // clearly points into a different allocation than the parent, refresh to the pointee's
    // allocation epoch (example: `&mut Vec<u8>` on the stack -> `Vec::as_mut_ptr()` heap buffer).
    let (mut alloc_epoch, mut alloc_live_at_creation, mut inherited_bounds_len) = if derived_from != 0 {
        let (parent_epoch, parent_live, parent_pointee, inherited_bounds_len, parent_parent) = tags()
            .lock()
            .unwrap()
            .get(&derived_from)
            .map(|p| {
                (
                    p.alloc_epoch,
                    p.alloc_live_at_creation,
                    Some(p.pointee_addr),
                    p.bounds_len,
                    p.parent,
                )
            })
            .unwrap_or((0, false, None, 0, 0));
        parent_is_root = parent_parent == 0;
        parent_pointee_addr = parent_pointee;

        if let Some(parent_pointee) = parent_pointee {
            let amap = allocs().lock().unwrap();
            let parent_alloc = find_alloc_containing(&amap, parent_pointee);
            let pointee_alloc = find_alloc_containing(&amap, pointee_addr);
            if let Some((pointee_base, pointee_meta)) = pointee_alloc {
                alloc_is_stack = pointee_meta.is_stack;
                alloc_size = pointee_meta.size;
                match parent_alloc {
                    Some((parent_base, _)) if parent_base == pointee_base => {
                        (parent_epoch, parent_live, inherited_bounds_len)
                    }
                    Some((_parent_base, _)) => {
                        parent_alloc_mismatch = true;
                        (pointee_meta.epoch, pointee_meta.live, 0)
                    }
                    None => {
                        // Parent alloc metadata can be missing in optimized lowering
                        // even when the child pointee alloc is known. Prefer the child
                        // alloc snapshot and allow exact-address lineage recovery below.
                        if parent_pointee != pointee_addr {
                            parent_alloc_mismatch = true;
                        }
                        (pointee_meta.epoch, pointee_meta.live, 0)
                    }
                }
            } else {
                (parent_epoch, parent_live, inherited_bounds_len)
            }
        } else {
            (parent_epoch, parent_live, inherited_bounds_len)
        }
    } else {
        // Root creation: if the match is a dead stack slot, treat metadata as unknown
        // to avoid inheriting stale bounds/epoch from recycled stack storage.
        let amap = allocs().lock().unwrap();
        match find_alloc_containing(&amap, pointee_addr) {
            Some((_base, m)) => {
                alloc_is_stack = m.is_stack;
                alloc_size = m.size;
                if m.is_stack && !m.live {
                    (0, false, 0)
                } else {
                    (m.epoch, m.live, 0)
                }
            }
            None => (0, false, 0),
        }
    };

    // Optimized MIR can materialize `&raw mut` from projected wrappers (e.g. Pin field access)
    // without a recoverable source local and emit `derived_from=0`. When this happens on stack
    // pointers, attach to a same-address recent non-root tag in the same epoch to preserve lineage.
    if resolved_parent == 0
        && projected_raw_hint
        && alloc_is_stack
        && alloc_epoch != 0
        && alloc_size >= std::mem::size_of::<usize>()
    {
        let requested_bounds = if bounds_len != 0 {
            bounds_len
        } else if carry_bounds_from_source {
            inherited_bounds_len
        } else {
            0
        };
        let repaired_parent = recover_parent_for_stack_root(
            pointee_addr,
            alloc_epoch,
            true,
            requested_bounds,
            projected_raw_strong_hint,
            matches!(kind, PtrKind::RawMut),
        );
        if repaired_parent != 0 {
            resolved_parent = repaired_parent;
            if let Some(parent_meta) = tags().lock().unwrap().get(&resolved_parent).cloned() {
                if inherited_bounds_len == 0 {
                    inherited_bounds_len = parent_meta.bounds_len;
                }
                if alloc_epoch == 0 && parent_meta.alloc_epoch != 0 {
                    alloc_epoch = parent_meta.alloc_epoch;
                    alloc_live_at_creation = parent_meta.alloc_live_at_creation;
                }
            }
            rz_trace!(
                "__record_raw_ptr_creation lineage repair: pointee=0x{:x} from={} -> {} epoch={} size={}",
                pointee_addr,
                derived_from,
                resolved_parent,
                alloc_epoch,
                alloc_size
            );
        }
    }

    // Derived pointer creation can still lose effective lineage in optimized async lowering:
    // a raw pointer may be emitted as "derived" from a freshly synthesized root that points to
    // a different stack slot than the raw pointee. In that shape we prefer exact same-address
    // non-root recovery over keeping the mismatched root parent.
    if resolved_parent != 0
        && parent_is_root
        && (parent_alloc_mismatch || parent_pointee_addr.map_or(false, |pp| pp != pointee_addr))
        && alloc_is_stack
        && alloc_epoch != 0
        && alloc_size >= std::mem::size_of::<usize>()
    {
        let requested_bounds = if bounds_len != 0 {
            bounds_len
        } else if carry_bounds_from_source {
            inherited_bounds_len
        } else {
            0
        };
        let repaired_parent = recover_parent_for_stack_root(
            pointee_addr,
            alloc_epoch,
            true,
            requested_bounds,
            projected_raw_strong_hint,
            matches!(kind, PtrKind::RawMut),
        );
        if repaired_parent != 0 && repaired_parent != resolved_parent {
            let previous_parent = resolved_parent;
            resolved_parent = repaired_parent;
            if let Some(parent_meta) = tags().lock().unwrap().get(&resolved_parent).cloned() {
                if inherited_bounds_len == 0 {
                    inherited_bounds_len = parent_meta.bounds_len;
                }
                if alloc_epoch == 0 && parent_meta.alloc_epoch != 0 {
                    alloc_epoch = parent_meta.alloc_epoch;
                    alloc_live_at_creation = parent_meta.alloc_live_at_creation;
                }
            }
            rz_trace!(
                "__record_raw_ptr_creation parent-mismatch repair: pointee=0x{:x} from={} {}->{} epoch={} size={}",
                pointee_addr,
                derived_from,
                previous_parent,
                resolved_parent,
                alloc_epoch,
                alloc_size
            );
        }
    }

    let bounds_len = if bounds_len != 0 {
        bounds_len
    } else if carry_bounds_from_source {
        inherited_bounds_len
    } else {
        0
    };

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: resolved_parent,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt_flag,
        lineage_hint: alias_exempt & 0b0000_1110,
        bounds_len,
    };
    tags().lock().unwrap().insert(tag, tmeta.clone());
    active_alias_model().on_tag_created(tag, &tmeta);

    let kind_str = match kind {
        PtrKind::RawConst => "const",
        PtrKind::RawMut => "mut",
        _ => "?",
    };
    rz_trace!(
        "__record_raw_ptr_creation called: tag={}, from={} (resolved={}), pointee=0x{:x}, kind={}, bounds={}",
        tag,
        derived_from,
        resolved_parent,
        pointee_addr,
        kind_str,
        bounds_len
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
    let _g = RzRuntimeGuard::enter();
    if tag == 0 {
        rz_trace!(
            "[rusteze-runtime] USE: untagged ptr addr=0x{:x} (likely untracked/propagation missing)",
            addr
        );
        return;
    }

    let mut tmap = tags().lock().unwrap();
    if let Some(tmeta) = tmap.get_mut(&tag) {
        tmeta.escaped = true;
        rz_trace!(
            "[rusteze-runtime] USE: tag={} addr=0x{:x} kind={:?} alloc_epoch={} parent={} escaped={}",
            tag,
            addr,
            tmeta.kind,
            tmeta.alloc_epoch,
            tmeta.parent,
            tmeta.escaped
        );
    } else {
        rz_trace!("[rusteze-runtime] USE: unknown tag={} addr=0x{:x}", tag, addr);
    }
}
