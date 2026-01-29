#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]
use core::ptr;
use std::sync::OnceLock;

mod static_image;
use static_image::StaticRange;

::std::thread_local! {
    // Re-entrancy guard to prevent infinite recursion when the runtime allocates
    // while recording allocation metadata.
    static RZ_IN_ALLOC_HOOK: ::std::cell::Cell<bool> = ::std::cell::Cell::new(false);

    // Guard to disable allocator recording while inside any runtime hook.
    // Logging (println!/format!) can allocate while locks are held.
    static RZ_IN_RUNTIME_HOOK: ::std::cell::Cell<u32> = ::std::cell::Cell::new(0);

    // Temporarily suppress SB-lite enforcement for coarse "unknown call" hooks.
    static RZ_SB_SUPPRESS: ::std::cell::Cell<bool> = ::std::cell::Cell::new(false);
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

#[inline]
fn rz_in_runtime_hook() -> bool {
    RZ_IN_RUNTIME_HOOK.with(|c| c.get() != 0)
}

#[inline]
fn rz_sb_suppressed() -> bool {
    RZ_SB_SUPPRESS.with(|c| c.get())
}

#[inline]
fn rz_abort_on_double_free() -> bool {
    // Default: abort on double free to avoid cascading UB/noise.
    std::env::var("RZ_ABORT_ON_DOUBLE_FREE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
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
}

static ALLOCS: OnceLock<Mutex<BTreeMap<usize, AllocMeta>>> = OnceLock::new();
static TAGS: OnceLock<Mutex<HashMap<u64, TagMeta>>> = OnceLock::new();
static BORROWS: OnceLock<Mutex<HashMap<usize, Vec<BorrowEntry>>>> = OnceLock::new();
static CALL_ARG_TAGS: OnceLock<Mutex<HashMap<(u64, u64, usize), u64>>> = OnceLock::new();
static RET_TAGS: OnceLock<Mutex<HashMap<(u64, usize), u64>>> = OnceLock::new();

fn allocs() -> &'static Mutex<BTreeMap<usize, AllocMeta>> {
    ALLOCS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn tags() -> &'static Mutex<HashMap<u64, TagMeta>> {
    TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn borrows() -> &'static Mutex<HashMap<usize, Vec<BorrowEntry>>> {
    BORROWS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BorrowKind {
    Shared,
    Unique,
}

#[derive(Clone, Debug)]
struct BorrowEntry {
    tag: u64,
    kind: BorrowKind,
}

#[inline]
fn rz_sb_lite_enabled() -> bool {
    std::env::var("RZ_SB_LITE")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
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
    // Choose the containing allocation with the largest end.

    let mut best: Option<(usize, &'a AllocMeta, usize)> = None; // (base, meta, end)
    let mut best_unknown: Option<(usize, &'a AllocMeta)> = None;

    for (base, meta) in amap.range(..=addr).rev() {
        let size = meta.size;

        if size == 0 {
            // Unknown-size allocations: only treat as containing if addr == base.
            // Keep as fallback only if we never find a known-size containing allocation.
            if *base == addr && best.is_none() {
                best_unknown = Some((*base, meta));
            }
            continue;
        }

        let end = match base.checked_add(size) {
            Some(e) => e,
            None => continue,
        };

        if addr < end {
            match best {
                None => best = Some((*base, meta, end)),
                Some((_b, _m, best_end)) => {
                    if end > best_end {
                        best = Some((*base, meta, end));
                    }
                }
            }
        }
    }

    if let Some((b, m, _end)) = best {
        Some((b, m))
    } else {
        best_unknown
    }
}

#[derive(Copy, Clone, Debug)]
enum AccessKind {
    Read,
    Write,
}

fn sb_lite_push(tag: u64, tmeta: &TagMeta) {
    if !rz_sb_lite_enabled() || tmeta.alias_exempt {
        return;
    }

    let kind = match tmeta.kind {
        PtrKind::RefShared => BorrowKind::Shared,
        PtrKind::RefMut => BorrowKind::Unique,
        _ => return,
    };

    let base = {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, tmeta.pointee_addr)
            .map(|(b, _)| b)
            .unwrap_or(tmeta.pointee_addr)
    };

    let mut bmap = borrows().lock().unwrap();
    let stack = bmap.entry(base).or_default();

    // Retagging: if we know the parent, truncate to it (invalidate younger tags).
    // For fresh unique borrows, clear the stack to invalidate all prior aliases.
    if tmeta.parent != 0 {
        if let Some(pos) = stack.iter().rposition(|entry| entry.tag == tmeta.parent) {
            stack.truncate(pos + 1);
        } else if kind == BorrowKind::Unique {
            stack.clear();
        }
    } else if kind == BorrowKind::Unique {
        stack.clear();
    }

    stack.push(BorrowEntry { tag, kind });
}

fn sb_lite_check(
    tag: u64,
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
    access: AccessKind,
) -> Option<String> {
    if !rz_sb_lite_enabled() || tmeta.alias_exempt || rz_sb_suppressed() {
        return None;
    }

    if !matches!(
        tmeta.kind,
        PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
    ) {
        return None;
    }

    let base = {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, addr)
            .map(|(b, _)| b)
            .unwrap_or(addr)
    };

    let mut bmap = borrows().lock().unwrap();
    let stack = match bmap.get_mut(&base) {
        Some(s) => s,
        None => return None,
    };

    let top = match stack.last() {
        Some(t) => t,
        None => return None,
    };

    match access {
        AccessKind::Read => {
            let mut seen_unique = false;
            for entry in stack.iter().rev() {
                if entry.tag == tag {
                    if seen_unique {
                        return Some(format!(
                            "READ via tag={tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                            tmeta.kind, top.kind, top.tag
                        ));
                    }
                    return None;
                }
                if entry.kind == BorrowKind::Unique {
                    seen_unique = true;
                }
            }
            Some(format!(
                "READ via tag={tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                tmeta.kind, top.kind, top.tag
            ))
        }
        AccessKind::Write => {
            let mut is_top = true;
            let mut seen_unique = false;
            for (idx, entry) in stack.iter().enumerate().rev() {
                if entry.tag == tag {
                    if entry.kind == BorrowKind::Unique {
                        if is_top {
                            return None;
                        }
                        // For raw writes derived from a unique ref, allow shared reborrows
                        // above as a best-effort heuristic (we do not track reborrow ends).
                        if matches!(tmeta.kind, PtrKind::RawMut) && !seen_unique {
                            return None;
                        }
                        // For unique refs, allow reactivation if only shared borrows are above.
                        // Heuristic: if the current top is shared, assume prior unique reborrows
                        // have ended and allow the parent unique to reactivate.
                        if matches!(tmeta.kind, PtrKind::RefMut)
                            && (!seen_unique || matches!(top.kind, BorrowKind::Shared))
                        {
                            stack.truncate(idx + 1);
                            return None;
                        }
                    }
                    return Some(format!(
                        "WRITE via tag={tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                        tmeta.kind, top.kind, top.tag
                    ));
                }
                if entry.kind == BorrowKind::Unique {
                    seen_unique = true;
                }
                if is_top {
                    is_top = false;
                }
            }
            Some(format!(
                "WRITE via tag={tag} addr=0x{addr:x} size={size} kind={:?}\nstack_top={:?} stack_tag={}",
                tmeta.kind, top.kind, top.tag
            ))
        }
    }
}

fn sb_lite_find_ref_ancestor_tag(
    tmap: &HashMap<u64, TagMeta>,
    mut tag: u64,
) -> Option<u64> {
    for _ in 0..32 {
        let t = tmap.get(&tag)?;
        if matches!(t.kind, PtrKind::RefShared | PtrKind::RefMut) {
            return Some(tag);
        }
        if t.parent == 0 {
            return None;
        }
        tag = t.parent;
    }
    None
}

#[inline(never)]
fn rz_violation(kind: &str, msg: String) {
    // Always print the report. Avoid stdio re-entrancy by writing directly to fd=2.
    rz_emit_str("\n================ RUSTEZE VIOLATION ================\n");
    rz_emit_str(kind);
    rz_emit_str("\n");
    rz_emit_str(&msg);
    if !msg.ends_with('\n') {
        rz_emit_str("\n");
    }
    rz_emit_str("===================================================\n\n");

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

    // Clear any SB-lite borrow stack when the allocation dies.
    if !new_live && rz_sb_lite_enabled() {
        drop(m);
        borrows().lock().unwrap().remove(&base_addr);
    }
}

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
    let _g = RzRuntimeGuard::enter();
    let (tmeta, sb_tag_opt) = {
        let tmap = tags().lock().unwrap();
        let Some(tmeta) = tmap.get(&tag) else {
            let msg = append_location_if_enabled(
                format!("WRITE unknown tag={tag} addr=0x{addr:x} size={size}"),
                "RZ_LOG_LOC",
            );
            let msg = append_backtrace_if_enabled(msg, "RZ_BACKTRACE_UNKNOWN_TAG");
            rz_violation(
                "UNKNOWN_TAG",
                msg,
            );
            return;
        };
        let sb_tag = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
            sb_lite_find_ref_ancestor_tag(&tmap, tag)
        } else {
            Some(tag)
        };
        (tmeta.clone(), sb_tag)
    };

    if let Some(sb_tag) = sb_tag_opt {
        if let Some(msg) = sb_lite_check(sb_tag, &tmeta, addr, size, AccessKind::Write) {
            rz_violation(
                "STACKED_BORROWS_VIOLATION",
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
        rz_violation(
            "USE_AFTER_DEAD",
            msg,
        );
        return;
    }

    if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
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
        rz_violation(
            "STALE_POINTER_EPOCH_MISMATCH",
            msg,
        );
        return;
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
    let _g = RzRuntimeGuard::enter();
    let (tmeta, sb_tag_opt) = {
        let tmap = tags().lock().unwrap();
        let Some(tmeta) = tmap.get(&tag) else {
            let msg = append_location_if_enabled(
                format!("READ unknown tag={tag} addr=0x{addr:x} size={size}"),
                "RZ_LOG_LOC",
            );
            let msg = append_backtrace_if_enabled(msg, "RZ_BACKTRACE_UNKNOWN_TAG");
            rz_violation(
                "UNKNOWN_TAG",
                msg,
            );
            return;
        };
        let sb_tag = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
            sb_lite_find_ref_ancestor_tag(&tmap, tag)
        } else {
            Some(tag)
        };
        (tmeta.clone(), sb_tag)
    };

    if let Some(sb_tag) = sb_tag_opt {
        if let Some(msg) = sb_lite_check(sb_tag, &tmeta, addr, size, AccessKind::Read) {
            rz_violation(
                "STACKED_BORROWS_VIOLATION",
                append_location_if_enabled(msg, "RZ_LOG_LOC"),
            );
            return;
        }
    }

    // Range-based allocation lookup.
    let amap = allocs().lock().unwrap();
    let alloc_opt = find_alloc_containing(&amap, addr);

    let Some((base, ameta)) = alloc_opt else {
        // If we can prove (via tag provenance + epoch snapshot) that this pointer was derived
        // from a particular allocation, classify this as OUT_OF_BOUNDS rather than WILD_POINTER.
        let tmap = tags().lock().unwrap();
        if let Some((obase, ometa)) = origin_alloc_for_tag(&tmap, &amap, tag) {
            if ometa.size != 0 && size != 0 {
                let access_end = addr.saturating_add(size);
                let alloc_end = obase.saturating_add(ometa.size);

                if addr >= obase && access_end > alloc_end {
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
        rz_violation(
            "USE_AFTER_DEAD",
            msg,
        );
        return;
    }

    if tmeta.alloc_epoch != 0 && ameta.epoch != 0 && tmeta.alloc_epoch != ameta.epoch {
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
        rz_violation(
            "STALE_POINTER_EPOCH_MISMATCH",
            msg,
        );
        return;
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
    call_arg_tags()
        .lock()
        .unwrap()
        .remove(&(callee_id, arg_index, addr))
        .unwrap_or(0)
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

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize, u8, u64, u8) -> u64 = $sym;
    };
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(
    pointee_addr: usize,
    is_mut: u8,
    parent_tag: u64,
    alias_exempt: u8,
) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RefMut } else { PtrKind::RefShared };

    // IMPORTANT: On retagging/reborrows (parent_tag != 0), we must NOT refresh alloc_epoch by
    // consulting the current allocation map, because the same numeric address can be reused by
    // a different stack frame. Derived tags should inherit the snapshot from their parent tag.
    let (alloc_epoch, alloc_live_at_creation) = if parent_tag != 0 {
        tags()
            .lock()
            .unwrap()
            .get(&parent_tag)
            .map(|p| (p.alloc_epoch, p.alloc_live_at_creation))
            .unwrap_or((0, false))
    } else {
        // Root creation: snapshot from the allocation that contains this address (range lookup).
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, pointee_addr)
            .map(|(_base, m)| (m.epoch, m.live))
            .unwrap_or((0, false))
    };

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: parent_tag,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt != 0,
    };
    tags().lock().unwrap().insert(tag, tmeta.clone());
    sb_lite_push(tag, &tmeta);

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
) -> u64 {
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 { PtrKind::RawMut } else { PtrKind::RawConst };

    // IMPORTANT: On retagging/derived pointers (derived_from != 0), prefer inheriting the
    // parent's alloc_epoch to keep the original allocation-instance snapshot and make
    // stack-slot reuse detectable as stale pointers. Exception: if the derived pointer
    // clearly points into a different allocation than the parent, refresh to the pointee's
    // allocation epoch (example: `&mut Vec<u8>` on the stack -> `Vec::as_mut_ptr()` heap buffer).
    let (alloc_epoch, alloc_live_at_creation) = if derived_from != 0 {
        let (parent_epoch, parent_live, parent_pointee) = tags()
            .lock()
            .unwrap()
            .get(&derived_from)
            .map(|p| (p.alloc_epoch, p.alloc_live_at_creation, Some(p.pointee_addr)))
            .unwrap_or((0, false, None));

        if let Some(parent_pointee) = parent_pointee {
            let amap = allocs().lock().unwrap();
            let parent_alloc = find_alloc_containing(&amap, parent_pointee);
            let pointee_alloc = find_alloc_containing(&amap, pointee_addr);
            if let (Some((parent_base, _)), Some((pointee_base, pointee_meta))) =
                (parent_alloc, pointee_alloc)
            {
                if parent_base != pointee_base {
                    (pointee_meta.epoch, pointee_meta.live)
                } else {
                    (parent_epoch, parent_live)
                }
            } else {
                (parent_epoch, parent_live)
            }
        } else {
            (parent_epoch, parent_live)
        }
    } else {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, pointee_addr)
            .map(|(_base, m)| (m.epoch, m.live))
            .unwrap_or((0, false))
    };

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: derived_from,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt != 0,
    };
    tags().lock().unwrap().insert(tag, tmeta);

    let kind_str = match kind {
        PtrKind::RawConst => "const",
        PtrKind::RawMut => "mut",
        _ => "?",
    };
    rz_trace!(
        "__record_raw_ptr_creation called: tag={}, from={}, pointee=0x{:x}, kind={}",
        tag,
        derived_from,
        pointee_addr,
        kind_str
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
