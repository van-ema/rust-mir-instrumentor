#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]
use core::ptr;
use std::sync::OnceLock;
#[cfg(feature = "runtime_hook_profile")]
use std::time::Instant;

#[cfg(all(feature = "runtime_hook_profile", unix))]
unsafe extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

mod static_image;
use static_image::StaticRange;
mod alias_model;
use alias_model::{active_alias_model, AliasAccessKind};
mod dead_epoch_cleanup;
mod live_alloc_cache;
mod ptr_shadow;
mod tag_history;
mod tag_lookup_cache;
mod tag_pruning;
mod tag_store;

// The pointee type is alias-exempt, usually because it contains interior mutability.
const CREATION_FLAG_ALIAS_EXEMPT: u8 = 0b0000_0001;
// Ref-only: the bounds length came from wide-pointer metadata and must be enforced eagerly.
const CREATION_FLAG_REF_DYNAMIC_BOUNDS: u8 = 0b0000_0010;
// Raw-only: keep exposed-provenance poison sticky when no tracked parent remains.
const CREATION_FLAG_RAW_PROPAGATE_EXPOSED_PROVENANCE: u8 = 0b0000_0010;
// The source is a projected helper path, so parent/slot shape matters to TB-lite.
const CREATION_FLAG_PROJECTED_HELPER_PARENT: u8 = 0b0000_0100;
// Ref-only bit 3: this ref reroots a returned by-value carrier in the caller.
const CREATION_FLAG_REF_RETURNED_CARRIER_REROOT: u8 = 0b0000_1000;
// Raw-only bit 3: carry precise wide-pointer bounds from the source pointer.
const CREATION_FLAG_RAW_CARRY_BOUNDS_FROM_SOURCE: u8 = 0b0000_1000;
// Raw-only: derived raw views reuse the parent TB family until a raw write needs a node.
const CREATION_FLAG_TB_RAW_REUSE_PARENT_FAMILY: u8 = 0b0001_0000;
// Raw-only: this is an int-to-ptr or exposed-provenance root.
const CREATION_FLAG_EXPOSED_PROVENANCE_ROOT: u8 = 0b0010_0000;
// Raw-only: validate this derived raw pointer immediately.
const CREATION_FLAG_STRICT_RAW_CREATION_CHECK: u8 = 0b0100_0000;
// Raw-only: this raw pointer came from a deref projection like `(*p).field`.
const CREATION_FLAG_DEREF_RAW_CREATION: u8 = 0b1000_0000;
const CREATION_REF_LINEAGE_HINT_MASK: u8 =
    CREATION_FLAG_PROJECTED_HELPER_PARENT | CREATION_FLAG_REF_RETURNED_CARRIER_REROOT;
const CREATION_RAW_LINEAGE_HINT_MASK: u8 =
    CREATION_FLAG_PROJECTED_HELPER_PARENT | CREATION_FLAG_TB_RAW_REUSE_PARENT_FAMILY;

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

#[cfg(feature = "runtime_hook_profile")]
struct HookProfileCounters {
    write_calls: AtomicU64,
    write_total_ns: AtomicU64,
    write_tag_lookup_ns: AtomicU64,
    write_alias_check_ns: AtomicU64,
    write_alloc_lookup_ns: AtomicU64,
    read_calls: AtomicU64,
    read_total_ns: AtomicU64,
    read_tag_lookup_ns: AtomicU64,
    read_alias_check_ns: AtomicU64,
    read_alloc_lookup_ns: AtomicU64,
    ref_create_calls: AtomicU64,
    ref_create_total_ns: AtomicU64,
    ref_create_validate_ns: AtomicU64,
    ref_create_alloc_snapshot_ns: AtomicU64,
    ref_create_insert_ns: AtomicU64,
    ref_create_tag_store_insert_ns: AtomicU64,
    ref_create_alias_on_tag_created_ns: AtomicU64,
    raw_create_calls: AtomicU64,
    raw_create_total_ns: AtomicU64,
    ptr_use_calls: AtomicU64,
    ptr_use_total_ns: AtomicU64,
    record_alloc_calls: AtomicU64,
    record_alloc_total_ns: AtomicU64,
    shadow_store_calls: AtomicU64,
    shadow_store_total_ns: AtomicU64,
    shadow_load_calls: AtomicU64,
    shadow_load_total_ns: AtomicU64,
    shadow_kill_range_calls: AtomicU64,
    shadow_kill_range_total_ns: AtomicU64,
    shadow_copy_slot_calls: AtomicU64,
    shadow_copy_slot_total_ns: AtomicU64,
    shadow_copy_range_calls: AtomicU64,
    shadow_copy_range_total_ns: AtomicU64,
    tag_retain_calls: AtomicU64,
    tag_retain_total_ns: AtomicU64,
    tag_kill_calls: AtomicU64,
    tag_kill_total_ns: AtomicU64,
    tag_kill_release_ns: AtomicU64,
    tag_kill_escape_check_ns: AtomicU64,
    tag_kill_alias_on_tag_killed_ns: AtomicU64,
    call_scope_calls: AtomicU64,
    call_scope_total_ns: AtomicU64,
    call_arg_push_calls: AtomicU64,
    call_arg_push_total_ns: AtomicU64,
    call_arg_zero_skipped: AtomicU64,
    call_arg_take_calls: AtomicU64,
    call_arg_take_total_ns: AtomicU64,
    call_arg_leaf_calls: AtomicU64,
    call_arg_leaf_total_ns: AtomicU64,
    mut_arg_ret_push_calls: AtomicU64,
    mut_arg_ret_push_total_ns: AtomicU64,
    mut_arg_ret_take_calls: AtomicU64,
    mut_arg_ret_take_total_ns: AtomicU64,
    ret_push_calls: AtomicU64,
    ret_push_total_ns: AtomicU64,
    ret_take_calls: AtomicU64,
    ret_take_total_ns: AtomicU64,
    fn_enter_calls: AtomicU64,
    fn_enter_total_ns: AtomicU64,
    fn_exit_calls: AtomicU64,
    fn_exit_total_ns: AtomicU64,
}

#[cfg(feature = "runtime_hook_profile")]
impl HookProfileCounters {
    const fn new() -> Self {
        Self {
            write_calls: AtomicU64::new(0),
            write_total_ns: AtomicU64::new(0),
            write_tag_lookup_ns: AtomicU64::new(0),
            write_alias_check_ns: AtomicU64::new(0),
            write_alloc_lookup_ns: AtomicU64::new(0),
            read_calls: AtomicU64::new(0),
            read_total_ns: AtomicU64::new(0),
            read_tag_lookup_ns: AtomicU64::new(0),
            read_alias_check_ns: AtomicU64::new(0),
            read_alloc_lookup_ns: AtomicU64::new(0),
            ref_create_calls: AtomicU64::new(0),
            ref_create_total_ns: AtomicU64::new(0),
            ref_create_validate_ns: AtomicU64::new(0),
            ref_create_alloc_snapshot_ns: AtomicU64::new(0),
            ref_create_insert_ns: AtomicU64::new(0),
            ref_create_tag_store_insert_ns: AtomicU64::new(0),
            ref_create_alias_on_tag_created_ns: AtomicU64::new(0),
            raw_create_calls: AtomicU64::new(0),
            raw_create_total_ns: AtomicU64::new(0),
            ptr_use_calls: AtomicU64::new(0),
            ptr_use_total_ns: AtomicU64::new(0),
            record_alloc_calls: AtomicU64::new(0),
            record_alloc_total_ns: AtomicU64::new(0),
            shadow_store_calls: AtomicU64::new(0),
            shadow_store_total_ns: AtomicU64::new(0),
            shadow_load_calls: AtomicU64::new(0),
            shadow_load_total_ns: AtomicU64::new(0),
            shadow_kill_range_calls: AtomicU64::new(0),
            shadow_kill_range_total_ns: AtomicU64::new(0),
            shadow_copy_slot_calls: AtomicU64::new(0),
            shadow_copy_slot_total_ns: AtomicU64::new(0),
            shadow_copy_range_calls: AtomicU64::new(0),
            shadow_copy_range_total_ns: AtomicU64::new(0),
            tag_retain_calls: AtomicU64::new(0),
            tag_retain_total_ns: AtomicU64::new(0),
            tag_kill_calls: AtomicU64::new(0),
            tag_kill_total_ns: AtomicU64::new(0),
            tag_kill_release_ns: AtomicU64::new(0),
            tag_kill_escape_check_ns: AtomicU64::new(0),
            tag_kill_alias_on_tag_killed_ns: AtomicU64::new(0),
            call_scope_calls: AtomicU64::new(0),
            call_scope_total_ns: AtomicU64::new(0),
            call_arg_push_calls: AtomicU64::new(0),
            call_arg_push_total_ns: AtomicU64::new(0),
            call_arg_zero_skipped: AtomicU64::new(0),
            call_arg_take_calls: AtomicU64::new(0),
            call_arg_take_total_ns: AtomicU64::new(0),
            call_arg_leaf_calls: AtomicU64::new(0),
            call_arg_leaf_total_ns: AtomicU64::new(0),
            mut_arg_ret_push_calls: AtomicU64::new(0),
            mut_arg_ret_push_total_ns: AtomicU64::new(0),
            mut_arg_ret_take_calls: AtomicU64::new(0),
            mut_arg_ret_take_total_ns: AtomicU64::new(0),
            ret_push_calls: AtomicU64::new(0),
            ret_push_total_ns: AtomicU64::new(0),
            ret_take_calls: AtomicU64::new(0),
            ret_take_total_ns: AtomicU64::new(0),
            fn_enter_calls: AtomicU64::new(0),
            fn_enter_total_ns: AtomicU64::new(0),
            fn_exit_calls: AtomicU64::new(0),
            fn_exit_total_ns: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "runtime_hook_profile")]
static RZ_HOOK_PROFILE: OnceLock<HookProfileCounters> = OnceLock::new();

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_hook_profile() -> &'static HookProfileCounters {
    rz_maybe_register_hook_profile_atexit();
    RZ_HOOK_PROFILE.get_or_init(HookProfileCounters::new)
}

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_profile_hooks_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RZ_PROFILE_HOOKS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_dump_hook_profile_at_exit_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RZ_DUMP_HOOK_PROFILE_AT_EXIT")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_profile_add_elapsed(counter: &AtomicU64, start: Instant) {
    let nanos = start.elapsed().as_nanos();
    let clipped = nanos.min(u64::MAX as u128) as u64;
    counter.fetch_add(clipped, Ordering::Relaxed);
}

#[cfg(all(feature = "runtime_hook_profile", unix))]
extern "C" fn rz_dump_hook_profile_atexit() {
    __rz_dump_hook_profile();
}

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_maybe_register_hook_profile_atexit() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    if !rz_profile_hooks_enabled() || !rz_dump_hook_profile_at_exit_enabled() {
        return;
    }
    let _ = REGISTERED.get_or_init(|| {
        #[cfg(unix)]
        unsafe {
            // Register once so repro/benchmark runs can dump aggregate runtime hook costs
            // without modifying individual harness binaries.
            let _ = atexit(rz_dump_hook_profile_atexit);
        }
    });
}

#[cfg(feature = "runtime_hook_profile")]
fn rz_elapsed_ns(start: Instant) -> u64 {
    let ns = start.elapsed().as_nanos();
    core::cmp::min(ns, u64::MAX as u128) as u64
}

#[cfg(feature = "runtime_hook_profile")]
#[inline]
fn rz_profile_add(counter: &AtomicU64, start: Option<Instant>) {
    if let Some(t0) = start {
        counter.fetch_add(rz_elapsed_ns(t0), Ordering::Relaxed);
    }
}

#[cfg(feature = "runtime_hook_profile")]
struct HookProfileGuard {
    start: Option<Instant>,
    total_counter: Option<&'static AtomicU64>,
}

#[cfg(feature = "runtime_hook_profile")]
impl HookProfileGuard {
    #[inline]
    fn write(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.write_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.write_total_ns),
        }
    }

    #[inline]
    fn read(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.read_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.read_total_ns),
        }
    }

    #[inline]
    fn ref_create(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.ref_create_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.ref_create_total_ns),
        }
    }

    #[inline]
    fn raw_create(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.raw_create_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.raw_create_total_ns),
        }
    }

    #[inline]
    fn ptr_use(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.ptr_use_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.ptr_use_total_ns),
        }
    }

    #[inline]
    fn record_alloc(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.record_alloc_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.record_alloc_total_ns),
        }
    }

    #[inline]
    fn shadow_store(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.shadow_store_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.shadow_store_total_ns),
        }
    }

    #[inline]
    fn shadow_load(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.shadow_load_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.shadow_load_total_ns),
        }
    }

    #[inline]
    fn shadow_kill_range(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.shadow_kill_range_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.shadow_kill_range_total_ns),
        }
    }

    #[inline]
    fn shadow_copy_slot(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.shadow_copy_slot_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.shadow_copy_slot_total_ns),
        }
    }

    #[inline]
    fn shadow_copy_range(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.shadow_copy_range_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.shadow_copy_range_total_ns),
        }
    }

    #[inline]
    fn tag_retain(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.tag_retain_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.tag_retain_total_ns),
        }
    }

    #[inline]
    fn tag_kill(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.tag_kill_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.tag_kill_total_ns),
        }
    }

    #[inline]
    fn call_scope(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.call_scope_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.call_scope_total_ns),
        }
    }

    #[inline]
    fn call_arg_push(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.call_arg_push_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.call_arg_push_total_ns),
        }
    }

    #[inline]
    fn call_arg_take(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.call_arg_take_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.call_arg_take_total_ns),
        }
    }

    #[inline]
    fn call_arg_leaf(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.call_arg_leaf_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.call_arg_leaf_total_ns),
        }
    }

    #[inline]
    fn mut_arg_ret_push(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.mut_arg_ret_push_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.mut_arg_ret_push_total_ns),
        }
    }

    #[inline]
    fn mut_arg_ret_take(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.mut_arg_ret_take_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.mut_arg_ret_take_total_ns),
        }
    }

    #[inline]
    fn ret_push(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.ret_push_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.ret_push_total_ns),
        }
    }

    #[inline]
    fn ret_take(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.ret_take_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.ret_take_total_ns),
        }
    }

    #[inline]
    fn fn_enter(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.fn_enter_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.fn_enter_total_ns),
        }
    }

    #[inline]
    fn fn_exit(profile: Option<&'static HookProfileCounters>) -> Self {
        let Some(p) = profile else {
            return Self {
                start: None,
                total_counter: None,
            };
        };
        p.fn_exit_calls.fetch_add(1, Ordering::Relaxed);
        Self {
            start: Some(Instant::now()),
            total_counter: Some(&p.fn_exit_total_ns),
        }
    }
}

#[cfg(feature = "runtime_hook_profile")]
impl Drop for HookProfileGuard {
    #[inline]
    fn drop(&mut self) {
        let (Some(t0), Some(total)) = (self.start.as_ref(), self.total_counter) else {
            return;
        };
        total.fetch_add(rz_elapsed_ns(*t0), Ordering::Relaxed);
    }
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_context {
    () => {
        rz_profile_hooks_enabled().then(rz_hook_profile)
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_context {
    () => {
        ()
    };
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_guard {
    ($profile:expr, $kind:ident) => {
        HookProfileGuard::$kind($profile)
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_guard {
    ($profile:expr, $kind:ident) => {
        ()
    };
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_start {
    ($profile:expr) => {
        $profile.map(|_| Instant::now())
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_start {
    ($profile:expr) => {
        ()
    };
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_add_elapsed_field {
    ($profile:expr, $field:ident, $start:expr) => {
        if let (Some(p), Some(start)) = ($profile, $start) {
            rz_profile_add_elapsed(&p.$field, start);
        }
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_add_elapsed_field {
    ($profile:expr, $field:ident, $start:expr) => {};
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_add_opt_field {
    ($profile:expr, $field:ident, $start:expr) => {
        if let Some(p) = $profile {
            rz_profile_add(&p.$field, $start);
        }
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_add_opt_field {
    ($profile:expr, $field:ident, $start:expr) => {};
}

#[cfg(feature = "runtime_hook_profile")]
macro_rules! rz_profile_inc_field {
    ($profile:expr, $field:ident) => {
        if let Some(p) = $profile {
            p.$field.fetch_add(1, Ordering::Relaxed);
        }
    };
}

#[cfg(not(feature = "runtime_hook_profile"))]
macro_rules! rz_profile_inc_field {
    ($profile:expr, $field:ident) => {};
}

#[cfg(feature = "runtime_ptr_shadow_trace")]
#[inline]
pub(crate) fn rz_trace_ptr_shadow_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RZ_TRACE_PTR_SHADOW")
            .ok()
            .is_some_and(|v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

#[cfg(not(feature = "runtime_ptr_shadow_trace"))]
#[inline]
pub(crate) fn rz_trace_ptr_shadow_enabled() -> bool {
    false
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
pub(crate) fn rz_stack_addr_hint(addr: usize) -> bool {
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
fn rz_strict_provenance_enabled() -> bool {
    std::env::var("RZ_STRICT_PROVENANCE")
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
    let stack_like = rz_stack_addr_hint(addr) || rz_stack_addr_hint(access_end.saturating_sub(1));
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
    if tmeta.alloc_epoch != 0 || !(rz_tls_addr_hint(tmeta.pointee_addr) || rz_tls_addr_hint(addr)) {
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
fn rz_validate_ref_creation_addr(
    pointee_addr: usize,
    kind: PtrKind,
    parent_tag: u64,
    bounds_len: usize,
    dynamic_bounds: bool,
) -> Option<(&'static str, String)> {
    let access_name = match kind {
        PtrKind::RefMut => "WRITE",
        PtrKind::RefShared => "READ",
        _ => "READ",
    };
    let access_len = bounds_len_bytes_or_zero(bounds_len).max(1);

    let suspicious_untracked_parent = tag_store::get(parent_tag)
        .as_ref()
        .is_some_and(|parent| parent.exposed_provenance_root);

    if pointee_addr == 0 && (parent_tag != 0 || suspicious_untracked_parent) {
        return Some((
            "WILD_POINTER",
            format!(
                "{access_name} via root ref create addr=0x0 size={access_len}\nreason=NULL_REF_CREATE kind={kind:?} parent={parent_tag}"
            ),
        ));
    }

    if let Some(r) = rz_static_range_for_addr(pointee_addr) {
        let access_end = pointee_addr.saturating_add(access_len);
        if access_end <= r.end {
            return None;
        }
    }

    if bounds_len_is_zero_sized_known(bounds_len) && pointee_addr != 0 {
        // Empty slice/str views and exact ZST refs may legally carry a dangling non-null pointer
        // so long as alignment was checked separately. Do not require allocation tracking for
        // these zero-sized creations; later concrete accesses still validate normally.
        return None;
    }

    if suspicious_untracked_parent {
        return Some((
            "WILD_POINTER",
            format!(
                "{access_name} via root ref create addr=0x{pointee_addr:x} size={access_len}\nreason=REF_CREATE_UNTRACKED kind={kind:?} parent={parent_tag}"
            ),
        ));
    }

    let (covering_alloc_opt, containing_alloc_opt) = {
        let amap = allocs().lock().unwrap();
        (
            find_alloc_covering_range(&amap, pointee_addr, access_len)
                .map(|(base, meta)| (base, *meta)),
            find_alloc_containing(&amap, pointee_addr).map(|(base, meta)| (base, *meta)),
        )
    };

    let has_covering_alloc = covering_alloc_opt.is_some();
    if let Some((base, ameta)) = covering_alloc_opt.or(containing_alloc_opt) {
        if !ameta.live {
            // Epoch bumps on every live/dead transition, so the natural sequence for a
            // single allocation is: N (live) -> N+1 (dead). When `ameta.epoch == parent.alloc_epoch + 1`
            // the tag is still pinned to the allocation instance that just died — real UAD.
            // A larger gap (`>= 2`) means the slot was reborn and died again in between,
            // which is the address-reuse false-positive pattern for stack frames.
            if ameta.is_stack {
                let parent_meta = tag_store::get(parent_tag);
                if parent_meta.as_ref().is_some_and(|parent| {
                    parent.parent != 0
                        && !parent.exposed_provenance_root
                        && parent.alloc_epoch == ameta.epoch
                        && parent.pointee_addr >= base
                        && (ameta.size == 0
                            || parent.pointee_addr < base.saturating_add(ameta.size))
                }) {
                    // Creating an interior stack reborrow from an existing live-tag family is
                    // too early to call UAD under optimized MIR. Stack-slot liveness can be more
                    // stale/coarse than the borrow lineage here; defer to the subsequent concrete
                    // read/write checks instead of failing at ref creation.
                    return None;
                }
                // Stack-slot live/dead metadata is too coarse under optimized MIR to treat ref
                // creation itself as a definitive UAD signal. Defer stack cases to later concrete
                // accesses instead of reporting from this boundary check.
                return None;
            }
            return Some((
                "USE_AFTER_DEAD",
                format!(
                    "{access_name} via root ref create addr=0x{pointee_addr:x} size={access_len}\nreason=REF_CREATE_FROM_DEAD_ALLOC alloc_base=0x{base:x} alloc_size={} alloc_epoch={} kind={kind:?} parent={parent_tag}",
                    ameta.size,
                    ameta.epoch,
                ),
            ));
        }

        if ameta.size != 0 {
            let access_end = pointee_addr.saturating_add(access_len);
            let alloc_end = base.saturating_add(ameta.size);
            if access_end > alloc_end {
                if parent_tag == 0
                    && !has_covering_alloc
                    && !dynamic_bounds
                    && (ameta.is_stack
                        || rz_stack_addr_hint(pointee_addr)
                        || rz_tls_addr_hint(pointee_addr))
                {
                    // Optimized MIR can expose only an overlapping stack/TLS slot for a larger
                    // root ref. Dynamic wide-pointer metadata is different: forged metadata can
                    // make a real reference too large, so those creations keep the eager OOB check.
                    return None;
                }
                return Some((
                    "OUT_OF_BOUNDS",
                    format!(
                        "{access_name} via root ref create addr=0x{pointee_addr:x} size={access_len}\nreason=REF_CREATE_OOB alloc_base=0x{base:x} alloc_end=0x{alloc_end:x} alloc_size={} kind={kind:?} parent={parent_tag}",
                        ameta.size,
                    ),
                ));
            }
        }

        return None;
    }

    if rz_untracked_region_for_access(pointee_addr, access_len).is_some() {
        return None;
    }

    // Best-effort policy: missing stack/TLS alloc metadata is common enough under optimized MIR
    // that we should not reject current-frame or TLS references purely because the alloc map is
    // incomplete. Concrete accesses still validate normally once the pointer is used.
    if rz_stack_addr_hint(pointee_addr) || rz_tls_addr_hint(pointee_addr) {
        return None;
    }

    Some((
        "WILD_POINTER",
        format!(
            "{access_name} via root ref create addr=0x{pointee_addr:x} size={access_len}\nreason=REF_CREATE_NO_ALLOC kind={kind:?} parent={parent_tag}"
        ),
    ))
}

#[inline]
fn rz_validate_strict_raw_creation_addr(
    pointee_addr: usize,
    kind: PtrKind,
    parent_tag: u64,
    exposed_provenance_root: bool,
    strict_provenance: bool,
    deref_projection_check: bool,
    bounds_len: usize,
) -> Option<(&'static str, String)> {
    // Raw-pointer creation should reject missing provenance, and should still catch the common
    // case of deriving an out-of-bounds raw from an in-bounds parent. However, some libraries
    // intentionally use already-out-of-bounds raw values as integer metadata carriers and later
    // reverse the arithmetic before any dereference (for example, `bytes` stores small offsets
    // in pointer-typed fields and reconstructs the real base pointer in `rebuild_vec`).
    // In that shape, eager OOB-on-derive is too strong: once the parent is already outside its
    // origin range, defer bounds enforcement to actual access / ref creation.
    let Some(mut parent_meta) = tag_store::get(parent_tag) else {
        if exposed_provenance_root && (strict_provenance || deref_projection_check) {
            return Some((
                "WILD_POINTER",
                format!(
                    "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason=NO_PROVENANCE_DERIVE kind={kind:?} parent={parent_tag}"
                ),
            ));
        }
        return None;
    };

    let same_zero_sized_family = bounds_len_is_zero_sized_known(bounds_len)
        && bounds_len_is_zero_sized_known(parent_meta.bounds_len)
        && parent_meta.pointee_addr == pointee_addr;
    let same_zero_sized_metadata_family = same_zero_sized_family
        && parent_meta.parent != 0
        && matches!(
            parent_meta.kind,
            PtrKind::RefShared | PtrKind::RefMut | PtrKind::RawConst | PtrKind::RawMut
        )
        && rz_can_recover_parent_tag(parent_tag);
    if same_zero_sized_family
        && (!rz_has_exposed_provenance_root(parent_tag, &parent_meta)
            || same_zero_sized_metadata_family)
    {
        // Extending the same zero-sized family at the same address is metadata transport, not a
        // concrete byte access. This covers both empty wide views (`&[]`, empty `str`) and exact
        // ZST families (`&Cell<()>`, `&()`) so helper plumbing can carry the lineage without
        // inventing fresh exposed-provenance roots. Real zero-sized forgeries still fail: there
        // must be an existing live family member to inherit, and changing the address leaves the
        // normal strict checks in place.
        return None;
    }

    if exposed_provenance_root && strict_provenance {
        return Some((
            "WILD_POINTER",
            format!(
                "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason=NO_PROVENANCE_DERIVE kind={kind:?} parent={parent_tag}"
            ),
        ));
    }

    // Parent tags cache allocation-origin bounds at creation time. For same-base realloc growth,
    // that cached origin can become stale within the same allocation epoch (for example
    // `BytesMut` growing from 8 to 16 bytes in place). Refresh the parent's origin from the live
    // alloc map before classifying a raw derive as OOB so we do not reject valid derives against
    // the resized allocation.
    if let Some((base, ameta)) = alloc_from_origin_base(&parent_meta) {
        let epoch_matches = parent_meta.alloc_epoch == 0
            || ameta.epoch == 0
            || parent_meta.alloc_epoch == ameta.epoch;
        if epoch_matches {
            refresh_tag_origin_cache(parent_tag, &mut parent_meta, base, &ameta);
        }
    }

    if rz_has_exposed_provenance_root(parent_tag, &parent_meta) {
        if strict_provenance {
            return Some((
                "WILD_POINTER",
                format!(
                    "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason=NO_PROVENANCE_DERIVE kind={kind:?} parent={parent_tag}\nparent_pointee=0x{:x}",
                    parent_meta.pointee_addr
                ),
            ));
        }
        if deref_projection_check && parent_meta.parent != 0 {
            let access_len = bounds_len_bytes_or_zero(bounds_len).max(1);
            if let Err(reason) = validate_exposed_live_alloc_access(
                parent_meta.pointee_addr,
                parent_meta.alloc_epoch,
                pointee_addr,
                access_len,
            ) {
                return Some((
                    reason.violation_kind(),
                    format!(
                        "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason={} kind={kind:?} parent={parent_tag}\nparent_pointee=0x{:x}",
                        reason.label(),
                        parent_meta.pointee_addr
                    ),
                ));
            }
        } else if deref_projection_check {
            return Some((
                "WILD_POINTER",
                format!(
                    "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason=NO_PROVENANCE_DERIVE kind={kind:?} parent={parent_tag}\nparent_pointee=0x{:x}",
                    parent_meta.pointee_addr
                ),
            ));
        }
    }

    let parent_already_oob = parent_meta.origin_known
        && parent_meta.origin_end > parent_meta.origin_base
        && (parent_meta.pointee_addr < parent_meta.origin_base
            || parent_meta.pointee_addr > parent_meta.origin_end);

    if !parent_already_oob
        && parent_meta.origin_known
        && parent_meta.origin_end > parent_meta.origin_base
        && (pointee_addr < parent_meta.origin_base || pointee_addr > parent_meta.origin_end)
    {
        return Some((
            "OUT_OF_BOUNDS",
            format!(
                "READ via raw derive addr=0x{pointee_addr:x} size=1\nreason=RAW_DERIVE_OOB origin_base=0x{:x} origin_end=0x{:x} kind={kind:?} parent={parent_tag}\nparent_pointee=0x{:x}",
                parent_meta.origin_base,
                parent_meta.origin_end,
                parent_meta.pointee_addr
            ),
        ));
    }

    None
}

#[inline]
fn rz_has_exposed_provenance_root(tag: u64, tmeta: &TagMeta) -> bool {
    if tmeta.exposed_provenance_root {
        return true;
    }

    let mut cur = tmeta.parent;
    let mut depth = 0usize;
    while cur != 0 && depth < 16 {
        let Some(parent) = tag_store::get(cur) else {
            break;
        };
        if parent.exposed_provenance_root {
            return true;
        }
        cur = parent.parent;
        depth += 1;
    }
    false
}

#[derive(Copy, Clone, Debug)]
enum ExposedAllocAccessError {
    AnchorNoAlloc,
    AnchorDead,
    AnchorEpochMismatch,
    AccessNoAlloc,
    AccessDead,
    AccessDifferentAlloc,
    AccessOutOfBounds,
}

impl ExposedAllocAccessError {
    fn label(self) -> &'static str {
        match self {
            Self::AnchorNoAlloc => "EXPOSED_ANCHOR_NO_ALLOC",
            Self::AnchorDead => "EXPOSED_ANCHOR_DEAD_ALLOC",
            Self::AnchorEpochMismatch => "EXPOSED_ANCHOR_EPOCH_MISMATCH",
            Self::AccessNoAlloc => "EXPOSED_ACCESS_NO_ALLOC",
            Self::AccessDead => "EXPOSED_ACCESS_DEAD_ALLOC",
            Self::AccessDifferentAlloc => "EXPOSED_ACCESS_DIFFERENT_ALLOC",
            Self::AccessOutOfBounds => "EXPOSED_ACCESS_OOB",
        }
    }

    fn violation_kind(self) -> &'static str {
        match self {
            Self::AnchorDead | Self::AccessDead | Self::AnchorEpochMismatch => "USE_AFTER_DEAD",
            Self::AccessDifferentAlloc | Self::AccessOutOfBounds => "OUT_OF_BOUNDS",
            Self::AnchorNoAlloc | Self::AccessNoAlloc => "WILD_POINTER",
        }
    }
}

#[inline]
fn validate_exposed_live_alloc_access(
    anchor_addr: usize,
    anchor_epoch: u64,
    access_addr: usize,
    access_len: usize,
) -> Result<(usize, AllocMeta), ExposedAllocAccessError> {
    let access_len = access_len.max(1);
    let _access_end = access_addr
        .checked_add(access_len)
        .ok_or(ExposedAllocAccessError::AccessOutOfBounds)?;

    let amap = allocs().lock().unwrap();
    let Some((anchor_base, anchor_meta)) = find_alloc_containing(&amap, anchor_addr) else {
        return Err(ExposedAllocAccessError::AnchorNoAlloc);
    };
    if !anchor_meta.live {
        return Err(ExposedAllocAccessError::AnchorDead);
    }
    if anchor_epoch != 0 && anchor_meta.epoch != 0 && anchor_epoch != anchor_meta.epoch {
        return Err(ExposedAllocAccessError::AnchorEpochMismatch);
    }

    let Some((access_base, access_meta)) =
        find_alloc_covering_range(&amap, access_addr, access_len)
    else {
        if let Some((containing_base, containing_meta)) = find_alloc_containing(&amap, access_addr)
        {
            if !containing_meta.live {
                return Err(ExposedAllocAccessError::AccessDead);
            }
            if containing_base != anchor_base {
                return Err(ExposedAllocAccessError::AccessDifferentAlloc);
            }
            return Err(ExposedAllocAccessError::AccessOutOfBounds);
        }
        return Err(ExposedAllocAccessError::AccessNoAlloc);
    };
    if !access_meta.live {
        return Err(ExposedAllocAccessError::AccessDead);
    }
    if access_base != anchor_base {
        return Err(ExposedAllocAccessError::AccessDifferentAlloc);
    }

    Ok((access_base, *access_meta))
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
        let ok = rz_pre_free_check(ptr, layout.size(), layout.align());
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use std::thread::ThreadId;

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
        Self {
            buf: [0u8; 1024],
            len: 0,
        }
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
fn rz_pre_free_check(ptr: *mut u8, layout_size: usize, _layout_align: usize) -> bool {
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
            // Two cases:
            //   (a) Legit: alloc performed while inside a runtime hook (recording suppressed).
            //       Those allocs go through `System.alloc` directly and land at real heap
            //       addresses (>= one page).
            //   (b) Invalid-free: user code passed a bogus pointer to dealloc, e.g.
            //       `Box::from_raw(NonNull::<T>::dangling().as_ptr())` where the "pointer"
            //       is just `align_of::<T>()` (a small integer well below any page).
            //
            // Heuristic: if base is implausibly small for a real heap address and the free
            // is non-ZST, treat it as INVALID_FREE. `RZ_STRICT_FREE_CHECK=1` promotes all
            // untracked frees to violations.
            let strict = std::env::var("RZ_STRICT_FREE_CHECK")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

            let looks_like_sentinel = layout_size > 0 && base < 0x10000;

            if strict || looks_like_sentinel {
                rz_violation(
                    "INVALID_FREE",
                    format!(
                        "FREE of unknown base=0x{base:x} size={layout_size} (skipping system dealloc to avoid abort)"
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

            // Layout-size mismatch: alloc recorded with one size, dealloc called with another
            // (e.g. `Box::from_raw(ptr as *mut u32)` when the alloc was `u16`).
            if layout_size != 0 && meta.size != 0 && layout_size != meta.size {
                rz_violation(
                    "DEALLOC_LAYOUT_MISMATCH",
                    format!(
                        "DEALLOC_LAYOUT_MISMATCH base=0x{base:x} alloc_size={} dealloc_size={}",
                        meta.size, layout_size
                    ),
                );
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
#[derive(Copy, Clone, Debug)]
pub struct AllocMeta {
    /// Whether the allocation is currently live.
    pub live: bool,
    /// Monotonically increasing epoch to disambiguate address reuse.
    pub epoch: u64,
    /// Optional size in bytes (0 if unknown).
    pub size: usize,
    /// Whether this allocation came from stack tracking.
    pub is_stack: bool,
    /// Whether this allocation came from a global/promoted const pointer materialization.
    pub is_const: bool,
}

/// Kind of pointer/tag we are tracking.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PtrKind {
    RefShared,
    RefMut,
    RawConst,
    RawMut,
}

pub(crate) const BOUNDS_LEN_UNKNOWN: usize = usize::MAX;
pub(crate) const BOUNDS_LEN_EXACT_ZST: usize = usize::MAX - 1;

#[inline]
pub(crate) fn bounds_len_is_unknown(bounds_len: usize) -> bool {
    bounds_len == BOUNDS_LEN_UNKNOWN
}

#[inline]
pub(crate) fn bounds_len_is_exact_zst(bounds_len: usize) -> bool {
    bounds_len == BOUNDS_LEN_EXACT_ZST
}

#[inline]
pub(crate) fn bounds_len_is_known(bounds_len: usize) -> bool {
    !bounds_len_is_unknown(bounds_len)
}

#[inline]
pub(crate) fn bounds_len_has_explicit_byte_bounds(bounds_len: usize) -> bool {
    bounds_len_is_known(bounds_len) && !bounds_len_is_exact_zst(bounds_len)
}

#[inline]
pub(crate) fn bounds_len_is_known_nonempty(bounds_len: usize) -> bool {
    bounds_len_has_explicit_byte_bounds(bounds_len) && bounds_len != 0
}

#[inline]
pub(crate) fn bounds_len_is_precise_empty(bounds_len: usize) -> bool {
    bounds_len_has_explicit_byte_bounds(bounds_len) && bounds_len == 0
}

#[inline]
pub(crate) fn bounds_len_is_zero_sized_known(bounds_len: usize) -> bool {
    bounds_len_is_precise_empty(bounds_len) || bounds_len_is_exact_zst(bounds_len)
}

#[inline]
pub(crate) fn bounds_len_bytes_or_zero(bounds_len: usize) -> usize {
    if bounds_len_has_explicit_byte_bounds(bounds_len) {
        bounds_len
    } else {
        0
    }
}

/// Metadata associated with a borrow tag.
#[derive(Copy, Clone, Debug)]
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
    /// Best-effort snapshot used to disambiguate address reuse
    /// (e.g., stack slots or freed heap memory).
    pub alloc_epoch: u64,
    /// Whether the tag was created while the containing allocation was live.
    pub alloc_live_at_creation: bool,
    /// Skip aliasing checks for tags pointing into UnsafeCell / interior mutability.
    pub alias_exempt: bool,
    /// Structural creation hints emitted by instrumentation.
    /// bit2=projected helper parent, bit3=returned-carrier reroot for refs, bit4=TB-lite raw
    /// reuses the parent family until an actual raw write needs access-local raw state,
    /// bit5=internal runtime normalization for const refs materialized at alloc end.
    pub lineage_hint: u8,
    /// Root raw pointer came from exposed-provenance/int-to-ptr creation.
    pub exposed_provenance_root: bool,
    /// Optional bounds length encoding for pointers/references.
    /// - `BOUNDS_LEN_UNKNOWN` => unknown / not provided
    /// - `0` => precise empty wide view (`&[]`, empty `str`, ...)
    /// - `BOUNDS_LEN_EXACT_ZST` => exact zero-sized thin pointee (`&()`, `&Cell<()>`, ...)
    /// - any other value => explicit byte length
    pub bounds_len: usize,
    /// Optional writable extent for pointers derived from shared interior-mutability roots.
    /// When nonzero, raw writes may extend beyond the exact pointee bounds but must stay inside
    /// this explicit enclosing extent.
    pub interior_mut_extent_base: usize,
    pub interior_mut_extent_len: usize,
    /// Best-effort required alignment for this pointer/reference in bytes.
    /// 0 means unknown and falls back to parent/access-specific metadata.
    pub align_req: usize,
    /// Whether we captured an allocation-origin snapshot for this tag.
    pub origin_known: bool,
    /// Base address of the allocation that originated this tag.
    pub origin_base: usize,
    /// End address of the allocation that originated this tag (half-open).
    /// For unknown-size allocations this is equal to `origin_base`.
    pub origin_end: usize,
}

static ALLOCS: OnceLock<Mutex<BTreeMap<usize, AllocMeta>>> = OnceLock::new();
static TAGS: OnceLock<Mutex<HashMap<u64, TagMeta>>> = OnceLock::new();
#[derive(Copy, Clone, Debug, Default)]
struct CallArgTagEntry {
    tag: u64,
    flags: u8,
}

#[derive(Debug, Default)]
struct DynamicCallArgScope {
    arg_tags: HashMap<(u64, usize), CallArgTagEntry>,
    leaf_shadows: HashMap<(u64, u64), CallArgLeafTransport>,
    consumed: bool,
}

#[derive(Copy, Clone, Debug)]
struct CallBoundaryActivation {
    callee_id: u64,
    boundary_id: u64,
    direct: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum BoundarySurvivorKind {
    /// A returned tag that writes back into a mutable argument's caller-visible family.
    MutArgRet,
    /// A normal returned tag whose family must survive call-exit cleanup.
    Return,
    /// A pointer-field tag inside a returned aggregate.
    ReturnLeaf,
}

/// One tag that must remain usable after the callee boundary is closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct BoundarySurvivorRecord {
    tag: u64,
    addr: usize,
    kind: BoundarySurvivorKind,
}

const CALL_ARG_FLAG_INPLACE_EXACT_SOURCE: u8 = 1;
// A callee still consumes the explicit boundary parent, but TB-lite must not
// turn that parent into a protected child for unresolved/generic `&mut Self`.
const CALL_ARG_FLAG_NO_PROTECTOR: u8 = 1 << 1;
const CALL_ARG_BOUNDARY_ORIGIN_EXACT: u8 = 0;

static CALL_ARG_TAGS: OnceLock<Mutex<HashMap<(ThreadId, u64, u64, usize), CallArgTagEntry>>> =
    OnceLock::new();
static PENDING_DIRECT_CALL_BOUNDARIES: OnceLock<Mutex<HashMap<ThreadId, Vec<u64>>>> =
    OnceLock::new();
static ACTIVE_CALL_BOUNDARIES: OnceLock<Mutex<HashMap<ThreadId, Vec<CallBoundaryActivation>>>> =
    OnceLock::new();
static CALL_ARG_LEAF_SHADOWS: OnceLock<
    Mutex<HashMap<(ThreadId, u64, u64, u64), CallArgLeafTransport>>,
> = OnceLock::new();
static RET_TAGS: OnceLock<Mutex<HashMap<(ThreadId, u64, usize), u64>>> = OnceLock::new();
type PtrShadowTransport = (u64, u64, u64, u8);
type CallArgLeafTransport = (usize, PtrShadowTransport);

static RET_LEAF_SHADOWS: OnceLock<Mutex<HashMap<(ThreadId, u64, u64), PtrShadowTransport>>> =
    OnceLock::new();
static MUT_ARG_RET_TAGS: OnceLock<Mutex<HashMap<(ThreadId, u64, u64, usize), u64>>> =
    OnceLock::new();
static MUT_ARG_RET_LEAF_SHADOWS: OnceLock<
    Mutex<HashMap<(ThreadId, u64, u64, usize, u64), PtrShadowTransport>>,
> = OnceLock::new();
static BOUNDARY_SURVIVOR_RECORDS: OnceLock<
    Mutex<HashMap<(ThreadId, u64), Vec<BoundarySurvivorRecord>>>,
> = OnceLock::new();
static MUT_ARG_RET_BOUNDARY_LINEAGES: OnceLock<Mutex<HashMap<ThreadId, HashSet<u64>>>> =
    OnceLock::new();
static CALL_ARG_LEAF_SCOPES: OnceLock<Mutex<HashMap<ThreadId, Vec<CallArgLeafScope>>>> =
    OnceLock::new();
static DYNAMIC_CALL_ARG_SCOPES: OnceLock<Mutex<HashMap<ThreadId, Vec<DynamicCallArgScope>>>> =
    OnceLock::new();
static PROMISED_ALIGNMENTS: OnceLock<Mutex<HashMap<(usize, u64), usize>>> = OnceLock::new();

#[derive(Debug)]
struct CallArgLeafScope {
    callee_id: u64,
    leaf_shadows: HashMap<(u64, usize), PtrShadowTransport>,
}

fn allocs() -> &'static Mutex<BTreeMap<usize, AllocMeta>> {
    ALLOCS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn tags() -> &'static Mutex<HashMap<u64, TagMeta>> {
    TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn promised_alignments() -> &'static Mutex<HashMap<(usize, u64), usize>> {
    PROMISED_ALIGNMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tag_alias_exempt_via_bounded_ancestor(tag: u64, addr: usize, size: usize) -> bool {
    let access_len = size.max(1);
    let access_end = addr.saturating_add(access_len);
    let tmap = tags().lock().unwrap();
    let mut cur = tag;

    for _ in 0..8 {
        let Some(meta) = tmap.get(&cur) else {
            break;
        };
        if meta.parent == 0 {
            break;
        }
        let Some(parent) = tmap.get(&meta.parent) else {
            break;
        };
        if parent.alias_exempt && bounds_len_is_known_nonempty(parent.bounds_len) {
            let parent_start = parent.pointee_addr;
            let parent_end = parent_start.saturating_add(parent.bounds_len);
            if addr >= parent_start && access_end <= parent_end {
                return true;
            }
        }
        cur = meta.parent;
    }

    false
}

#[inline]
fn rz_addr_alignment(addr: usize) -> usize {
    if addr == 0 {
        return 0;
    }
    1usize << addr.trailing_zeros()
}

#[inline]
fn rz_effective_align_req(requested_align: usize, parent_tag: u64) -> usize {
    let parent_align = if parent_tag == 0 {
        0
    } else {
        tag_store::get(parent_tag)
            .map(|meta| meta.align_req)
            .unwrap_or(0)
    };
    match (requested_align, parent_align) {
        (0, p) => p,
        (r, 0) => r,
        (r, p) => r.min(p),
    }
}

#[inline]
fn rz_effective_ref_align_req(
    requested_align: usize,
    parent_tag: u64,
    pointee_addr: usize,
) -> usize {
    let Some(parent) = (if parent_tag == 0 {
        None
    } else {
        tag_store::get(parent_tag)
    }) else {
        return requested_align;
    };
    if requested_align == 0 {
        return parent.align_req;
    }
    if parent.pointee_addr == pointee_addr {
        return match parent.align_req {
            0 => requested_align,
            parent_align => requested_align.min(parent_align),
        };
    }
    requested_align
}

fn rz_check_alignment(
    access_name: &str,
    tag: u64,
    addr: usize,
    size: usize,
    guaranteed_align: usize,
    required_align: usize,
    tmeta: Option<&TagMeta>,
) {
    if required_align <= 1 {
        return;
    }

    let concrete_aligned = addr != 0 && addr % required_align == 0;
    let live_heap_aligned = concrete_aligned
        && lookup_alloc_snapshot(addr)
            .is_some_and(|(_, meta)| meta.live && !meta.is_stack && !meta.is_const);

    if guaranteed_align != 0 && guaranteed_align < required_align && !live_heap_aligned {
        let mut msg = format!(
            "{access_name} via tag={tag} addr=0x{addr:x} size={size}\nrequired_alignment={required_align} guaranteed_alignment={guaranteed_align}"
        );
        if let Some(meta) = tmeta {
            use std::fmt::Write as _;
            let _ = write!(
                msg,
                "\nkind={:?} parent={} pointee=0x{:x}",
                meta.kind, meta.parent, meta.pointee_addr
            );
        }
        rz_violation(
            "MISALIGNED_ACCESS",
            append_location_if_enabled(msg, "RZ_LOG_LOC"),
        );
        return;
    }

    // Heap raws are checked dynamically: a weak tag alignment is not a violation when the
    // concrete live heap address satisfies the access alignment.
    if addr == 0 || concrete_aligned {
        return;
    }

    let found_align = rz_addr_alignment(addr);
    let mut msg = format!(
        "{access_name} via tag={tag} addr=0x{addr:x} size={size}\nrequired_alignment={required_align} guaranteed_alignment={guaranteed_align} found_alignment={found_align}"
    );
    if let Some(meta) = tmeta {
        use std::fmt::Write as _;
        let _ = write!(
            msg,
            "\nkind={:?} parent={} pointee=0x{:x}",
            meta.kind, meta.parent, meta.pointee_addr
        );
    }
    rz_violation(
        "MISALIGNED_ACCESS",
        append_location_if_enabled(msg, "RZ_LOG_LOC"),
    );
}

fn call_arg_tags() -> &'static Mutex<HashMap<(ThreadId, u64, u64, usize), CallArgTagEntry>> {
    CALL_ARG_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending_direct_call_boundaries() -> &'static Mutex<HashMap<ThreadId, Vec<u64>>> {
    PENDING_DIRECT_CALL_BOUNDARIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_call_boundaries() -> &'static Mutex<HashMap<ThreadId, Vec<CallBoundaryActivation>>> {
    ACTIVE_CALL_BOUNDARIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_call_boundary_id(callee_id: u64) -> u64 {
    active_call_boundary_activation(callee_id)
        .map(|activation| activation.boundary_id)
        .unwrap_or(callee_id)
}

fn active_call_boundary_activation(callee_id: u64) -> Option<CallBoundaryActivation> {
    let thread_id = std::thread::current().id();
    active_call_boundaries()
        .lock()
        .unwrap()
        .get(&thread_id)
        .and_then(|stack| {
            stack
                .iter()
                .rev()
                .find(|activation| activation.callee_id == callee_id)
        })
        .copied()
}

/// Return true while a direct or indirect call boundary is still on this thread's stack.
fn active_call_boundary_is_open(boundary_id: u64) -> bool {
    let thread_id = std::thread::current().id();
    active_call_boundaries()
        .lock()
        .unwrap()
        .get(&thread_id)
        .is_some_and(|stack| {
            stack
                .iter()
                .any(|activation| activation.boundary_id == boundary_id)
        })
}

fn enter_call_boundary(callee_id: u64) -> u64 {
    let thread_id = std::thread::current().id();
    let (boundary_id, direct) = {
        let mut pending = pending_direct_call_boundaries().lock().unwrap();
        let popped = pending.get_mut(&thread_id).and_then(|stack| stack.pop());
        if pending
            .get(&thread_id)
            .is_some_and(|stack| stack.is_empty())
        {
            pending.remove(&thread_id);
        }
        match popped {
            Some(id) => (id, true),
            None => (callee_id, false),
        }
    };
    active_call_boundaries()
        .lock()
        .unwrap()
        .entry(thread_id)
        .or_default()
        .push(CallBoundaryActivation {
            callee_id,
            boundary_id,
            direct,
        });
    boundary_id
}

fn exit_call_boundary(callee_id: u64) -> CallBoundaryActivation {
    let thread_id = std::thread::current().id();
    let mut active = active_call_boundaries().lock().unwrap();
    let Some(stack) = active.get_mut(&thread_id) else {
        return CallBoundaryActivation {
            callee_id,
            boundary_id: callee_id,
            direct: false,
        };
    };
    let activation = stack
        .iter()
        .rposition(|activation| activation.callee_id == callee_id)
        .map(|idx| stack.remove(idx))
        .unwrap_or(CallBoundaryActivation {
            callee_id,
            boundary_id: callee_id,
            direct: false,
        });
    if stack.is_empty() {
        active.remove(&thread_id);
    }
    activation
}

fn exit_direct_call_boundary(boundary_id: u64) -> Option<CallBoundaryActivation> {
    let thread_id = std::thread::current().id();
    let mut active = active_call_boundaries().lock().unwrap();
    let stack = active.get_mut(&thread_id)?;
    let activation = stack
        .iter()
        .rposition(|activation| activation.direct && activation.boundary_id == boundary_id)
        .map(|idx| stack.remove(idx));
    if stack.is_empty() {
        active.remove(&thread_id);
    }
    activation
}

/// Close one call boundary in TB order: clear temporary import state, end protectors, then replay exports.
fn finish_call_boundary_activation(activation: CallBoundaryActivation) {
    // Direct calls export return tags before the caller imports them. Run protector teardown
    // first, then repair only the tags explicitly recorded as boundary survivors.
    clear_unconsumed_call_arg_leaf_shadows(activation.boundary_id);
    exit_call_arg_leaf_scope(activation.callee_id);
    active_alias_model().on_call_exit(activation.boundary_id);
    apply_deferred_boundary_survivor_exports(activation.boundary_id);
    clear_boundary_survivor_tags(activation.boundary_id);
}

fn finalize_direct_call_boundary(boundary_id: u64) {
    if let Some(activation) = exit_direct_call_boundary(boundary_id) {
        finish_call_boundary_activation(activation);
    }
}

/// Replay tags that were exported before call-exit so they survive protector teardown.
fn apply_deferred_boundary_survivor_exports(callee_id: u64) {
    // Replay export hooks after call-exit so TB-lite does not clear protectors too early.
    for record in boundary_survivor_records_for_callee(callee_id) {
        let export_addr = if record.addr != 0 {
            record.addr
        } else {
            tag_store::get(record.tag)
                .map(|meta| meta.pointee_addr)
                .unwrap_or(0)
        };
        match record.kind {
            BoundarySurvivorKind::MutArgRet => {
                active_alias_model().on_mut_arg_ret_export(record.tag, export_addr);
            }
            BoundarySurvivorKind::Return | BoundarySurvivorKind::ReturnLeaf => {
                active_alias_model().on_ret_export(record.tag, export_addr);
            }
        }
    }
}

fn call_arg_leaf_shadows(
) -> &'static Mutex<HashMap<(ThreadId, u64, u64, u64), CallArgLeafTransport>> {
    CALL_ARG_LEAF_SHADOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn call_arg_leaf_scopes() -> &'static Mutex<HashMap<ThreadId, Vec<CallArgLeafScope>>> {
    CALL_ARG_LEAF_SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dynamic_call_arg_scopes() -> &'static Mutex<HashMap<ThreadId, Vec<DynamicCallArgScope>>> {
    DYNAMIC_CALL_ARG_SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn capture_call_arg_leaf_shadows_for_arg(
    thread_id: ThreadId,
    callee_id: u64,
    matched_callee_id: u64,
    arg_index: u64,
) {
    let captured: Vec<((u64, usize), PtrShadowTransport)> = {
        let mut leafs = call_arg_leaf_shadows().lock().unwrap();
        let keys: Vec<_> = leafs
            .keys()
            .filter(|(tid, cid, idx, _key)| {
                *tid == thread_id && *cid == matched_callee_id && *idx == arg_index
            })
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|key| {
                let leaf_key = key.3;
                leafs
                    .remove(&key)
                    .map(|(slot_addr, shadow)| ((leaf_key, slot_addr), shadow))
            })
            .collect()
    };
    if captured.is_empty() {
        return;
    }

    let mut scopes = call_arg_leaf_scopes().lock().unwrap();
    let stack = scopes.entry(thread_id).or_default();
    if !stack
        .last()
        .is_some_and(|scope| scope.callee_id == callee_id)
    {
        stack.push(CallArgLeafScope {
            callee_id,
            leaf_shadows: HashMap::new(),
        });
    }
    if let Some(scope) = stack.last_mut() {
        scope.leaf_shadows.extend(captured);
    }
}

fn current_slot_shadow(slot_addr: usize) -> Option<PtrShadowTransport> {
    let ptr_addr = shadow_slot_value_addr(slot_addr);
    let shadow = (
        ptr_shadow::load_tag_for_ptr_value(slot_addr, ptr_addr),
        ptr_shadow::load_ref_ancestor_for_ptr_value(slot_addr, ptr_addr),
        ptr_shadow::load_export_parent_for_ptr_value(slot_addr, ptr_addr),
        ptr_shadow::load_export_parent_recovered_for_ptr_value(slot_addr, ptr_addr),
    );
    (shadow.0 != 0 || shadow.1 != 0 || shadow.2 != 0).then_some(shadow)
}

fn shadow_is_present(shadow: PtrShadowTransport) -> bool {
    shadow.0 != 0 || shadow.1 != 0 || shadow.2 != 0
}

fn matching_leaf_shadow(
    slot_addr: usize,
    shadow: PtrShadowTransport,
) -> Option<PtrShadowTransport> {
    if !shadow_is_present(shadow) {
        return None;
    }
    let sanitized =
        sanitize_shadow_entry_for_slot_value(slot_addr, shadow.0, shadow.1, shadow.2, shadow.3);
    shadow_is_present(sanitized).then_some(sanitized)
}

fn restore_leaf_shadow(slot_addr: usize, shadow: Option<PtrShadowTransport>) {
    let Some((tag, ref_ancestor, export_parent, export_parent_recovered)) =
        shadow.and_then(|shadow| matching_leaf_shadow(slot_addr, shadow))
    else {
        return;
    };
    // Leaf side-channels repair missing metadata; they do not model a byte write.
    ptr_shadow::store_ptr(
        slot_addr,
        tag,
        ref_ancestor,
        export_parent,
        export_parent_recovered,
    );
}

fn recovered_return_ref_leaf_shadow(slot_addr: usize) -> Option<PtrShadowTransport> {
    let ptr_addr = shadow_slot_value_addr(slot_addr);
    if ptr_addr == 0 {
        return None;
    }
    let tag = recover_live_ref_boundary_tag_for_ptr_value(ptr_addr);
    if tag != 0 {
        if rz_trace_call_tags_enabled() {
            eprintln!(
                "[rusteze-runtime][call-tag] recover ret leaf slot=0x{:x} ptr=0x{:x} tag={}",
                slot_addr, ptr_addr, tag
            );
        }
        return Some((tag, tag, tag, 0));
    }
    None
}

fn canonical_return_leaf_shadow(slot_addr: usize, leaf_is_ref: bool) -> PtrShadowTransport {
    let Some((tag, ref_ancestor, export_parent, export_parent_recovered)) =
        current_slot_shadow(slot_addr)
    else {
        if leaf_is_ref {
            if let Some(shadow) = recovered_return_ref_leaf_shadow(slot_addr) {
                return shadow;
            }
        }
        return (0, 0, 0, 0);
    };
    if export_parent_recovered != 0 && export_parent != 0 {
        // Do not return a callee-local helper tag; return its boundary parent.
        (export_parent, export_parent, export_parent, 0)
    } else {
        (tag, ref_ancestor, export_parent, export_parent_recovered)
    }
}

fn scoped_call_arg_leaf_shadow(
    thread_id: ThreadId,
    leaf_key: u64,
    slot_addr: usize,
) -> Option<PtrShadowTransport> {
    let scoped = {
        let scopes = call_arg_leaf_scopes().lock().unwrap();
        let stack = scopes.get(&thread_id)?;
        let mut found = None;
        for scope in stack.iter().rev() {
            if let Some(shadow) = scope.leaf_shadows.get(&(leaf_key, slot_addr)).copied() {
                found = Some(shadow);
                break;
            }

            // If the callee stack slot moved, a unique leaf key is still exact enough.
            let mut by_key = scope
                .leaf_shadows
                .iter()
                .filter_map(|((key, _source_slot), shadow)| (*key == leaf_key).then_some(*shadow));
            if let Some(first) = by_key.next() {
                if by_key.next().is_none() {
                    found = Some(first);
                }
                break;
            }
        }
        found?
    };
    current_slot_shadow(slot_addr).or_else(|| matching_leaf_shadow(slot_addr, scoped))
}

fn clear_unconsumed_call_arg_leaf_shadows(callee_id: u64) {
    let thread_id = std::thread::current().id();
    call_arg_leaf_shadows()
        .lock()
        .unwrap()
        .retain(|(tid, cid, _idx, _key), _shadow| *tid != thread_id || *cid != callee_id);
}

fn enter_call_arg_leaf_scope(callee_id: u64) {
    let thread_id = std::thread::current().id();
    call_arg_leaf_scopes()
        .lock()
        .unwrap()
        .entry(thread_id)
        .or_default()
        .push(CallArgLeafScope {
            callee_id,
            leaf_shadows: HashMap::new(),
        });
}

fn exit_call_arg_leaf_scope(callee_id: u64) {
    let thread_id = std::thread::current().id();
    let mut scopes = call_arg_leaf_scopes().lock().unwrap();
    let Some(stack) = scopes.get_mut(&thread_id) else {
        return;
    };
    if stack
        .last()
        .is_some_and(|scope| scope.callee_id == callee_id)
    {
        stack.pop();
    } else if let Some(pos) = stack.iter().rposition(|scope| scope.callee_id == callee_id) {
        stack.remove(pos);
    }
    if stack.is_empty() {
        scopes.remove(&thread_id);
    }
}

pub(crate) fn ret_tags() -> &'static Mutex<HashMap<(ThreadId, u64, usize), u64>> {
    RET_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn ret_leaf_shadows() -> &'static Mutex<HashMap<(ThreadId, u64, u64), PtrShadowTransport>>
{
    RET_LEAF_SHADOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn mut_arg_ret_tags() -> &'static Mutex<HashMap<(ThreadId, u64, u64, usize), u64>> {
    MUT_ARG_RET_TAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn mut_arg_ret_leaf_shadows(
) -> &'static Mutex<HashMap<(ThreadId, u64, u64, usize, u64), PtrShadowTransport>> {
    MUT_ARG_RET_LEAF_SHADOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return only the survivor tag IDs for alias-model call-exit code that does not need their kind.
pub(crate) fn boundary_survivor_tags_for_callee(callee_id: u64) -> HashSet<u64> {
    let thread_id = std::thread::current().id();
    boundary_survivor_records()
        .lock()
        .unwrap()
        .get(&(thread_id, callee_id))
        .map(|records| records.iter().map(|record| record.tag).collect())
        .unwrap_or_default()
}

fn boundary_survivor_records(
) -> &'static Mutex<HashMap<(ThreadId, u64), Vec<BoundarySurvivorRecord>>> {
    BOUNDARY_SURVIVOR_RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mut_arg_ret_boundary_lineages() -> &'static Mutex<HashMap<ThreadId, HashSet<u64>>> {
    MUT_ARG_RET_BOUNDARY_LINEAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return the full survivor records so replay can distinguish returns from mut-arg-ret exports.
fn boundary_survivor_records_for_callee(callee_id: u64) -> Vec<BoundarySurvivorRecord> {
    let thread_id = std::thread::current().id();
    boundary_survivor_records()
        .lock()
        .unwrap()
        .get(&(thread_id, callee_id))
        .cloned()
        .unwrap_or_default()
}

/// Remember that `tag` crossed this boundary and must be re-exported after call-exit.
fn remember_boundary_survivor(callee_id: u64, tag: u64, addr: usize, kind: BoundarySurvivorKind) {
    if tag == 0 {
        return;
    }
    // Keep the survivor kind: ordinary returns and mut-arg-ret writeback have different TB meaning.
    let thread_id = std::thread::current().id();
    let mut records = boundary_survivor_records().lock().unwrap();
    let entry = records.entry((thread_id, callee_id)).or_default();
    let record = BoundarySurvivorRecord { tag, addr, kind };
    if !entry.contains(&record) {
        entry.push(record);
    }
}

fn remember_mut_arg_ret_boundary_lineage(tag: u64) {
    if tag == 0 {
        return;
    }
    let thread_id = std::thread::current().id();
    mut_arg_ret_boundary_lineages()
        .lock()
        .unwrap()
        .entry(thread_id)
        .or_default()
        .insert(tag);
}

fn clear_boundary_survivor_tags(callee_id: u64) {
    let thread_id = std::thread::current().id();
    boundary_survivor_records()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id));
}

fn tag_is_in_mut_arg_ret_boundary_lineage(tag: u64) -> bool {
    if tag == 0 {
        return false;
    }
    let thread_id = std::thread::current().id();
    let roots = mut_arg_ret_boundary_lineages()
        .lock()
        .unwrap()
        .get(&thread_id)
        .cloned()
        .unwrap_or_default();
    if roots.is_empty() {
        return false;
    }

    let tmap = tags().lock().unwrap();
    let mut cur = tag;
    for _ in 0..64 {
        if roots.contains(&cur) {
            return true;
        }
        let Some(meta) = tmap.get(&cur) else {
            return false;
        };
        if meta.parent == 0 || meta.parent == cur {
            return false;
        }
        cur = meta.parent;
    }
    false
}

fn return_tag_is_mut_arg_ret_boundary_survivor(tag: u64) -> bool {
    tag != 0
        && active_alias_model().name() == "tb_lite"
        && tag_is_in_mut_arg_ret_boundary_lineage(tag)
}

/// Apply a boundary export now, or store it until the callee's call-exit cleanup has run.
fn apply_or_defer_boundary_survivor(
    callee_id: u64,
    tag: u64,
    addr: usize,
    kind: BoundarySurvivorKind,
) {
    if tag == 0 {
        return;
    }
    remember_boundary_survivor(callee_id, tag, addr, kind);
    if active_call_boundary_is_open(callee_id) {
        // The callee has not run alias-model exit yet; repair will happen after protector teardown.
        return;
    }

    let export_addr = if addr != 0 {
        addr
    } else {
        tag_store::get(tag)
            .map(|meta| meta.pointee_addr)
            .unwrap_or(0)
    };
    match kind {
        BoundarySurvivorKind::MutArgRet => {
            active_alias_model().on_mut_arg_ret_export(tag, export_addr);
        }
        BoundarySurvivorKind::Return | BoundarySurvivorKind::ReturnLeaf => {
            active_alias_model().on_ret_export(tag, export_addr);
        }
    }
}

/// Export a return tag, deferring the alias-model replay if the boundary is still open.
fn export_return_tag(callee_id: u64, tag: u64, addr: usize, survivor_kind: BoundarySurvivorKind) {
    if tag == 0 {
        return;
    }
    if matches!(survivor_kind, BoundarySurvivorKind::MutArgRet) {
        remember_mut_arg_ret_boundary_lineage(tag);
    }
    apply_or_defer_boundary_survivor(callee_id, tag, addr, survivor_kind);
}

/// Validate the returned reference at the boundary, then record it as a survivor.
fn validate_and_export_return_tag(
    callee_id: u64,
    tag: u64,
    addr: usize,
    survivor_kind: BoundarySurvivorKind,
) {
    let kind = tag_store::get(tag).map(|meta| meta.kind);
    if matches!(kind, Some(PtrKind::RefMut))
        && !matches!(survivor_kind, BoundarySurvivorKind::MutArgRet)
    {
        // Returning `&mut` is a boundary retag. It must still have unique authority, but the
        // diagnostic remains a boundary READ because no caller write has happened yet.
        rz_validate_mut_ref_boundary_retag(tag, "RET");
    } else {
        rz_validate_ref_boundary_use(tag, "RET");
    }
    export_return_tag(callee_id, tag, addr, survivor_kind);
}

#[inline]
fn rz_promised_alignment_for_addr(addr: usize, alloc_epoch: u64) -> usize {
    let map = promised_alignments().lock().unwrap();
    map.get(&(addr, alloc_epoch))
        .copied()
        .or_else(|| map.get(&(addr, 0)).copied())
        .unwrap_or(0)
}

/// Read the dynamic alignment from a live `&dyn Trait` fat-pointer local.
fn rz_dyn_ref_align_from_slot(slot_addr: usize) -> usize {
    if slot_addr == 0 {
        return 0;
    }
    let word = std::mem::size_of::<usize>();
    let vtable_slot = slot_addr.saturating_add(word);

    // `slot_addr` is emitted by instrumentation as the live local that stores the fat pointer.
    // Read its vtable word directly; then validate the vtable before reading its header.
    let vtable = unsafe { std::ptr::read_unaligned(vtable_slot as *const usize) };
    if vtable == 0
        || (rz_static_range_for_addr(vtable).is_none() && lookup_alloc_snapshot(vtable).is_none())
    {
        return 0;
    }
    let align_slot = vtable.saturating_add(2 * word);
    // A Rust trait-object vtable header stores drop/size/align, so word 2 is the
    // dynamic pointee alignment that Miri checks for `&dyn Trait`.
    let align = unsafe { std::ptr::read_unaligned(align_slot as *const usize) };
    if align.is_power_of_two() {
        align
    } else {
        0
    }
}

/// Check alignment for untagged reference uses, where provenance was missing but MIR still has a ref.
fn rz_check_tagless_ref_alignment(addr: usize, align_req: usize, kind: PtrKind) {
    if align_req <= 1 || addr == 0 || addr % align_req == 0 {
        return;
    }
    let found_align = rz_addr_alignment(addr);
    rz_violation(
        "MISALIGNED_ACCESS",
        append_location_if_enabled(
            format!(
                "REF_USE via tag=0 addr=0x{addr:x} kind={kind:?}\nrequired_alignment={align_req} found_alignment={found_align}"
            ),
            "RZ_LOG_LOC",
        ),
    );
}

#[no_mangle]
pub extern "C" fn __rz_promise_symbolic_alignment(ptr: *const (), align: usize) {
    let _g = RzRuntimeGuard::enter();
    if !align.is_power_of_two() {
        let msg = append_location_if_enabled(
            format!("alignment must be a power of 2\nalign={align}"),
            "RZ_LOG_LOC",
        );
        rz_violation("MISALIGNED_ACCESS", msg);
        return;
    }

    let addr = ptr as usize;
    if addr != 0 && addr % align != 0 {
        let msg = append_location_if_enabled(
            format!(
                "pointer is not actually aligned\naddr=0x{addr:x} promised_alignment={align} found_alignment={}",
                rz_addr_alignment(addr)
            ),
            "RZ_LOG_LOC",
        );
        rz_violation("MISALIGNED_ACCESS", msg);
        return;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_, meta)| meta.epoch)
        .unwrap_or(0);
    let mut map = promised_alignments().lock().unwrap();
    map.entry((addr, alloc_epoch))
        .and_modify(|prev| *prev = (*prev).max(align))
        .or_insert(align);
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
            let slot = if meta.live {
                &mut best_live
            } else {
                &mut best_dead
            };
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

#[inline]
fn find_alloc_covering_range<'a>(
    amap: &'a BTreeMap<usize, AllocMeta>,
    addr: usize,
    size: usize,
) -> Option<(usize, &'a AllocMeta)> {
    // Like `find_alloc_containing`, but require full coverage of the requested range. This keeps
    // root ref creation from spuriously binding to a smaller overlapping stack slot.
    let access_end = addr.checked_add(size)?;
    let mut best_live: Option<(usize, &'a AllocMeta, usize)> = None;
    let mut best_dead: Option<(usize, &'a AllocMeta, usize)> = None;
    let mut best_unknown: Option<(usize, &'a AllocMeta)> = None;

    for (base, meta) in amap.range(..=addr).rev() {
        let alloc_size = meta.size;

        if alloc_size == 0 {
            if *base == addr && size == 0 && best_live.is_none() && best_dead.is_none() {
                best_unknown = Some((*base, meta));
            }
            continue;
        }

        let end = match base.checked_add(alloc_size) {
            Some(e) => e,
            None => continue,
        };

        if access_end <= end {
            let slot = if meta.live {
                &mut best_live
            } else {
                &mut best_dead
            };
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

/// Best-effort origin lookup for provenance-aware OOB classification.
///
/// Unlike direct access lookup, this also accepts the exact one-past-end address of a tracked
/// allocation. Example:
///   let p = v.as_ptr().add(v.len()); // legal to compute, illegal to dereference
/// A later read through `p` should be classified as OUT_OF_BOUNDS relative to `v`'s allocation,
/// not as a generic WILD_POINTER.
#[inline]
fn find_alloc_origin_candidate<'a>(
    amap: &'a BTreeMap<usize, AllocMeta>,
    addr: usize,
) -> Option<(usize, &'a AllocMeta)> {
    if let Some(found) = find_alloc_containing(amap, addr) {
        return Some(found);
    }

    let mut best_live: Option<(usize, &'a AllocMeta)> = None;
    let mut best_dead: Option<(usize, &'a AllocMeta)> = None;

    for (base, meta) in amap.range(..addr).rev() {
        if meta.size == 0 {
            continue;
        }
        let Some(end) = base.checked_add(meta.size) else {
            continue;
        };
        if end != addr {
            continue;
        }

        if meta.live {
            best_live = Some((*base, meta));
            break;
        }
        if best_dead.is_none() {
            best_dead = Some((*base, meta));
        }
    }

    best_live.or(best_dead)
}

#[inline]
pub(crate) fn lookup_alloc_snapshot(addr: usize) -> Option<(usize, AllocMeta)> {
    live_alloc_cache::lookup_containing(addr).or_else(|| {
        let amap = allocs().lock().unwrap();
        find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
    })
}

#[inline]
fn lookup_alloc_origin_snapshot(addr: usize) -> Option<(usize, AllocMeta)> {
    if let Some(found) = live_alloc_cache::lookup_containing(addr) {
        return Some(found);
    }
    let amap = allocs().lock().unwrap();
    find_alloc_origin_candidate(&amap, addr).map(|(base, meta)| (base, *meta))
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
    if !bounds_len_is_known_nonempty(tmeta.bounds_len) || size == 0 {
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
    if tmeta.parent != 0 || bounds_len_is_known(tmeta.bounds_len) {
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

    // Root-tagged stack raws are low-confidence when lineage is missing, but keep this
    // suppression narrow: tolerate only small near-boundary overhangs. Broad interior-slot
    // suppression masked real OOB writes such as:
    //   let q = (p as *mut u8).add(size_of::<S>() - 1);
    //   ptr::write_unaligned(q.add(8) as *mut u64, ...)
    // where `q` lands inside a tracked stack slot and the 8-byte write truly crosses the
    // containing allocation boundary.
    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    let near_boundary = addr >= alloc_end && addr.saturating_sub(alloc_end) <= 16;
    near_boundary && size <= 16 && access_end > alloc_end
}

#[inline]
fn rz_allow_stack_raw_nonroot_boundary_oob_noise(
    tmeta: &TagMeta,
    ameta: &AllocMeta,
    base: usize,
    addr: usize,
    size: usize,
) -> bool {
    if !matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        return false;
    }
    if tmeta.parent == 0 || bounds_len_is_known(tmeta.bounds_len) {
        return false;
    }
    if !(ameta.is_stack || rz_stack_addr_hint(addr) || rz_stack_addr_hint(tmeta.pointee_addr)) {
        return false;
    }

    let usize_sz = std::mem::size_of::<usize>();
    if ameta.size > usize_sz || size == 0 || size > usize_sz {
        return false;
    }

    let alloc_end = base.saturating_add(ameta.size);
    let access_end = addr.saturating_add(size);
    if addr != alloc_end || access_end <= alloc_end {
        return false;
    }

    let tmap = tags().lock().unwrap();
    let mut cur = tmeta.parent;
    let mut depth = 0usize;
    while cur != 0 && depth < 8 {
        let Some(parent) = tmap.get(&cur) else {
            break;
        };
        if matches!(parent.kind, PtrKind::RefShared)
            && parent.pointee_addr >= base
            && parent.pointee_addr < alloc_end
        {
            return true;
        }
        cur = parent.parent;
        depth += 1;
    }
    false
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
    // Projected helper-parent shape only; keep this path narrowly targeted.
    if (tmeta.lineage_hint & CREATION_FLAG_PROJECTED_HELPER_PARENT) == 0 || tmeta.parent == 0 {
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
    let failfast = std::env::var("RUSTEZE_FAILFAST")
        .ok()
        .map_or(false, |v| v != "0");
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

        if let Some((base, ameta)) = find_alloc_origin_candidate(amap, t.pointee_addr) {
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

#[inline]
fn origin_end_from_alloc(base: usize, ameta: &AllocMeta) -> usize {
    if ameta.size == 0 {
        base
    } else {
        base.saturating_add(ameta.size)
    }
}

#[inline]
fn tag_origin_contains_access(tmeta: &TagMeta, addr: usize, size: usize) -> bool {
    if !tmeta.origin_known || size == 0 {
        return false;
    }

    let access_end = match addr.checked_add(size) {
        Some(e) => e,
        None => return false,
    };

    if tmeta.origin_end > tmeta.origin_base {
        addr >= tmeta.origin_base && access_end <= tmeta.origin_end
    } else {
        // Unknown-size alloc snapshot: only exact-base accesses are trusted.
        addr == tmeta.origin_base
    }
}

#[inline]
fn tag_origin_oob_cached(tmeta: &TagMeta, addr: usize, size: usize) -> bool {
    if !tmeta.origin_known || size == 0 || tmeta.origin_end <= tmeta.origin_base {
        return false;
    }
    let access_end = match addr.checked_add(size) {
        Some(e) => e,
        None => return true,
    };
    addr < tmeta.origin_base || access_end > tmeta.origin_end
}

#[inline]
fn alloc_from_origin_base(tmeta: &TagMeta) -> Option<(usize, AllocMeta)> {
    if !tmeta.origin_known {
        return None;
    }
    let amap = allocs().lock().unwrap();
    amap.get(&tmeta.origin_base)
        .copied()
        .map(|ameta| (tmeta.origin_base, ameta))
}

#[inline]
fn refresh_tag_origin_cache(tag: u64, tmeta: &mut TagMeta, base: usize, ameta: &AllocMeta) {
    let origin_end = origin_end_from_alloc(base, ameta);
    if tmeta.origin_known && tmeta.origin_base == base && tmeta.origin_end == origin_end {
        return;
    }

    tmeta.origin_known = true;
    tmeta.origin_base = base;
    tmeta.origin_end = origin_end;
    tag_store::update(tag, *tmeta);
}

#[inline]
fn snapshot_tag_origin(pointee_addr: usize, parent_tag: u64) -> (bool, usize, usize) {
    if let Some((base, ameta)) = lookup_alloc_origin_snapshot(pointee_addr) {
        return (true, base, origin_end_from_alloc(base, &ameta));
    }

    if parent_tag != 0 {
        if let Some(parent_meta) = tag_store::get(parent_tag) {
            if parent_meta.origin_known {
                return (true, parent_meta.origin_base, parent_meta.origin_end);
            }
        }
    }
    (false, 0, 0)
}

#[inline]
fn normalize_const_end_ref_pointee(
    pointee_addr: usize,
    parent_tag: u64,
    bounds_len: usize,
) -> usize {
    if parent_tag != 0 || pointee_addr == 0 {
        return pointee_addr;
    }
    if lookup_alloc_snapshot(pointee_addr).is_some() {
        return pointee_addr;
    }
    let Some((base, meta)) = lookup_alloc_origin_snapshot(pointee_addr) else {
        return pointee_addr;
    };
    if !meta.is_const || meta.size == 0 {
        return pointee_addr;
    }
    let end = base.saturating_add(meta.size);
    if end == pointee_addr {
        // Optimized MIR can materialize root refs to promoted/string literals at the
        // allocation end instead of the true object start. For references, an exact
        // alloc-end address is never semantically valid, so normalize it back to the
        // allocation base even when we were not given an explicit bounds length.
        base
    } else {
        pointee_addr
    }
}

// Some optimized MIR exposes a const/static reference at the allocation end.
// Example: for const bytes `[a, b]`, the ref may look like `base + 2`.
// We normalize it to `base`, and this hint lets later checks translate accesses
// consistently instead of treating the normalized tag as pointing at the end.
const LINEAGE_HINT_CONST_END_REF_NORMALIZED: u8 = 0b0010_0000;
// Example: `q = p.add(1)` stays in `p`'s TB family; it is not a new raw authority.
const LINEAGE_HINT_TB_RAW_REUSE_PARENT_FAMILY: u8 = CREATION_FLAG_TB_RAW_REUSE_PARENT_FAMILY;

#[inline]
fn normalize_const_end_ref_access_addr(
    tmeta: &TagMeta,
    addr: usize,
    size: usize,
) -> (usize, usize) {
    if (tmeta.lineage_hint & LINEAGE_HINT_CONST_END_REF_NORMALIZED) == 0
        && tmeta.parent == 0
        && matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
        && tmeta.origin_known
        && tmeta.origin_end > tmeta.origin_base
        && tmeta.pointee_addr == tmeta.origin_end
    {
        if let Some((_base, meta)) = alloc_from_origin_base(tmeta) {
            if meta.is_const {
                let shift = tmeta.origin_end - tmeta.origin_base;
                if addr >= tmeta.pointee_addr {
                    return (addr.saturating_sub(shift), size);
                }
            }
        }
    }
    if (tmeta.lineage_hint & LINEAGE_HINT_CONST_END_REF_NORMALIZED) == 0 {
        return (addr, size);
    }
    if bounds_len_is_unknown(tmeta.bounds_len) {
        if tmeta.parent == 0
            && tmeta.origin_known
            && tmeta.origin_end > tmeta.origin_base
            && addr == tmeta.origin_end
        {
            let origin_size = tmeta.origin_end - tmeta.origin_base;
            if size != 0 && size <= origin_size {
                return (addr.saturating_sub(size), size);
            }
        }
        return (addr, size);
    }
    let shifted_base = tmeta.pointee_addr.saturating_add(tmeta.bounds_len);
    if addr < shifted_base {
        return (addr, size);
    }
    (addr.saturating_sub(tmeta.bounds_len), size)
}

/// Record (or update) allocation metadata. The key is the base address.
/// This is a building block; stack/heap instrumentation will call this later.
#[no_mangle]
pub extern "C" fn __rz_record_alloc(base_addr: usize, size: usize, live: u8) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, record_alloc);
    let _g = RzRuntimeGuard::enter();

    // Record allocation events into a fixed-size ring buffer for post-mortem dumps.
    alloc_log_record(base_addr, size, live);

    if rz_log_alloc_enabled() {
        // Emit allocation events regardless of RZ_LOG level.
        let new_live = (live & 0x1) != 0;
        let is_stack = (live & 0x2) != 0;
        rz_emit_alloc(format_args!(
            "[rusteze-runtime] record_alloc base=0x{:x} size={} live={} is_stack={} is_const={}",
            base_addr,
            size,
            new_live,
            is_stack,
            (live & 0x4) != 0
        ));
    } else if rz_log_enabled(LogLevel::Trace) {
        // `live` bit 0: live/dead. bit 1: stack marker.
        let new_live = (live & 0x1) != 0;
        let is_stack = (live & 0x2) != 0;
        rz_trace!(
            "[rusteze-runtime] record_alloc base=0x{:x} size={} live={} is_stack={} is_const={}",
            base_addr,
            size,
            new_live,
            is_stack,
            (live & 0x4) != 0
        );
    }

    let mut m = allocs().lock().unwrap();
    let is_stack = (live & 0x2) != 0;
    let is_const = (live & 0x4) != 0;
    let entry = m.entry(base_addr).or_insert(AllocMeta {
        live: false,
        epoch: 0,
        size,
        is_stack,
        is_const,
    });

    if is_stack {
        entry.is_stack = true;
    }
    if is_const {
        entry.is_const = true;
    }

    let new_live = (live & 0x1) != 0;
    // We treat `epoch` as an allocation-instance counter for a given base address.
    // We must bump it not only on death, but also on reuse (dead -> live), otherwise
    // a later allocation at the same numeric address could "revive" stale pointers.
    let compact_dead_epoch = if !new_live && entry.live {
        Some(entry.epoch)
    } else {
        None
    };

    // Death transition: live to dead
    if !new_live && entry.live {
        entry.epoch = entry.epoch.wrapping_add(1);
    }

    let was_live = entry.live;

    // Reuse/birth transition: dead to live at an address we've seen before.
    // If we already had a nonzero epoch, bump it so this is a fresh instance.
    if new_live && !was_live {
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

    // Size is tracked per allocation instance, not monotonically across address reuse.
    //
    // On a dead -> live transition, reset to the newly observed size so a fresh allocation
    // instance cannot inherit a stale larger range from an older epoch at the same base.
    // Within the same live epoch we still keep the maximum known size, since some hooks only
    // discover partial size information before a later hook reports the full extent.
    if size != 0 {
        if new_live && !was_live {
            entry.size = size;
        } else {
            entry.size = entry.size.max(size);
        }
    }

    let entry_snapshot = *entry;
    drop(m);
    live_alloc_cache::update_alloc(base_addr, entry_snapshot);
    active_alias_model().on_alloc_state_change(base_addr, new_live);
    if let Some(dead_epoch) = compact_dead_epoch {
        dead_epoch_cleanup::reclaim_alloc_epoch(base_addr, dead_epoch);
    }
}

#[inline]
fn shadow_slot_value_addr(slot_addr: usize) -> usize {
    if slot_addr == 0 {
        return 0;
    }
    // Shadow hooks run after the MIR assignment to `slot_addr`, so the concrete pointer bits are
    // already present in memory here.
    unsafe { ptr::read_unaligned(slot_addr as *const usize) }
}

#[inline]
fn shadow_ref_lineage_matches_value(tag: u64, value_addr: usize) -> bool {
    if tag == 0 || value_addr == 0 {
        return true;
    }

    let Some(meta) = tag_store::get(tag) else {
        return true;
    };
    if !matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return true;
    }
    if meta.pointee_addr == 0 || meta.pointee_addr == value_addr {
        return true;
    }

    let Some((value_base, value_alloc)) = lookup_alloc_snapshot(value_addr) else {
        return false;
    };
    let Some((meta_base, meta_alloc)) = lookup_alloc_snapshot(meta.pointee_addr) else {
        return meta.align_req <= 1 || value_addr % meta.align_req == 0;
    };
    value_base == meta_base
        && (value_alloc.epoch == meta_alloc.epoch
            || value_alloc.epoch == 0
            || meta_alloc.epoch == 0)
}

#[inline]
fn sanitize_shadow_entry_for_slot_value(
    slot_addr: usize,
    tag: u64,
    ref_ancestor: u64,
    export_parent: u64,
    export_parent_recovered: u8,
) -> PtrShadowTransport {
    let value_addr = shadow_slot_value_addr(slot_addr);
    if shadow_ref_lineage_matches_value(tag, value_addr)
        && shadow_ref_lineage_matches_value(ref_ancestor, value_addr)
        && shadow_ref_lineage_matches_value(export_parent, value_addr)
    {
        return (tag, ref_ancestor, export_parent, export_parent_recovered);
    }

    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] sanitize slot=0x{:x} value=0x{:x} tag={} ref_ancestor={} export_parent={} recovered={}",
            slot_addr, value_addr, tag, ref_ancestor, export_parent, export_parent_recovered
        );
    }
    (0, 0, 0, 0)
}

#[no_mangle]
pub extern "C" fn __rz_shadow_store_ptr(
    slot_addr: usize,
    tag: u64,
    ref_ancestor: u64,
    export_parent: u64,
    export_parent_recovered: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_store);
    let _g = RzRuntimeGuard::enter();
    let (tag, ref_ancestor, export_parent, export_parent_recovered) =
        sanitize_shadow_entry_for_slot_value(
            slot_addr,
            tag,
            ref_ancestor,
            export_parent,
            export_parent_recovered,
        );
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] store slot=0x{:x} tag={} ref_ancestor={} export_parent={} recovered={}",
            slot_addr, tag, ref_ancestor, export_parent, export_parent_recovered
        );
    }
    ptr_shadow::store_ptr(
        slot_addr,
        tag,
        ref_ancestor,
        export_parent,
        export_parent_recovered,
    );
}

#[no_mangle]
pub extern "C" fn __rz_shadow_store_ptr_local(
    slot_addr: usize,
    tag: u64,
    ref_ancestor: u64,
    export_parent: u64,
    export_parent_recovered: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_store);
    let _g = RzRuntimeGuard::enter();
    let (tag, ref_ancestor, export_parent, export_parent_recovered) =
        sanitize_shadow_entry_for_slot_value(
            slot_addr,
            tag,
            ref_ancestor,
            export_parent,
            export_parent_recovered,
        );
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] store_local slot=0x{:x} tag={} ref_ancestor={} export_parent={} recovered={}",
            slot_addr, tag, ref_ancestor, export_parent, export_parent_recovered
        );
    }
    ptr_shadow::store_ptr_local_slot(
        slot_addr,
        tag,
        ref_ancestor,
        export_parent,
        export_parent_recovered,
    );
}

#[no_mangle]
pub extern "C" fn __rz_shadow_store_alloc_root(
    slot_addr: usize,
    ptr_addr: usize,
    is_mut: u8,
    bounds_len: usize,
    align_req: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_store);
    let _g = RzRuntimeGuard::enter();
    if slot_addr == 0 {
        return;
    }
    if ptr_addr == 0 {
        ptr_shadow::store_ptr(slot_addr, 0, 0, 0, 0);
        return;
    }

    // Allocation constructors produce a new owner pointer. Its provenance starts at the
    // current allocation instance, even if the allocator reused an old numeric address.
    let tag = __record_raw_ptr_creation(ptr_addr, is_mut, 0, 0, bounds_len, align_req);
    ptr_shadow::store_ptr(slot_addr, tag, 0, tag, 0);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] store_alloc_root slot=0x{:x} ptr=0x{:x} tag={}",
            slot_addr, ptr_addr, tag
        );
    }
}

#[no_mangle]
pub extern "C" fn __rz_shadow_store_external_alloc_root(
    slot_addr: usize,
    ptr_addr: usize,
    is_mut: u8,
    bounds_len: usize,
    align_req: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_store);
    let _g = RzRuntimeGuard::enter();
    if slot_addr == 0 {
        return;
    }
    if ptr_addr == 0 {
        ptr_shadow::store_ptr(slot_addr, 0, 0, 0, 0);
        return;
    }

    let Some((_base, alloc_meta)) = lookup_alloc_snapshot(ptr_addr) else {
        ptr_shadow::store_ptr(slot_addr, 0, 0, 0, 0);
        return;
    };
    if !alloc_meta.live {
        ptr_shadow::store_ptr(slot_addr, 0, 0, 0, 0);
        return;
    }

    // External owner imports are for allocations created by uninstrumented std code and recorded
    // by the allocator wrapper. A zero-capacity Vec has no live allocation, so it stays untagged.
    let tag = __record_raw_ptr_creation(ptr_addr, is_mut, 0, 0, bounds_len, align_req);
    ptr_shadow::store_ptr(slot_addr, tag, 0, tag, 0);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] store_external_alloc_root slot=0x{:x} ptr=0x{:x} tag={}",
            slot_addr, ptr_addr, tag
        );
    }
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_tag(slot_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let tag = ptr_shadow::load_tag(slot_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_tag slot=0x{:x} -> {}",
            slot_addr, tag
        );
    }
    tag
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_tag_for_ptr(slot_addr: usize, ptr_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let tag = ptr_shadow::load_tag_for_ptr_value(slot_addr, ptr_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_tag_for_ptr slot=0x{:x} ptr=0x{:x} -> {}",
            slot_addr, ptr_addr, tag
        );
    }
    tag
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_ref_ancestor(slot_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let ref_ancestor = ptr_shadow::load_ref_ancestor(slot_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_ref_ancestor slot=0x{:x} -> {}",
            slot_addr, ref_ancestor
        );
    }
    ref_ancestor
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_ref_ancestor_for_ptr(slot_addr: usize, ptr_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let ref_ancestor = ptr_shadow::load_ref_ancestor_for_ptr_value(slot_addr, ptr_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_ref_ancestor_for_ptr slot=0x{:x} ptr=0x{:x} -> {}",
            slot_addr, ptr_addr, ref_ancestor
        );
    }
    ref_ancestor
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_export_parent(slot_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let export_parent = ptr_shadow::load_export_parent(slot_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_export_parent slot=0x{:x} -> {}",
            slot_addr, export_parent
        );
    }
    export_parent
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_export_parent_for_ptr(slot_addr: usize, ptr_addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let export_parent = ptr_shadow::load_export_parent_for_ptr_value(slot_addr, ptr_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_export_parent_for_ptr slot=0x{:x} ptr=0x{:x} -> {}",
            slot_addr, ptr_addr, export_parent
        );
    }
    export_parent
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_export_parent_recovered(slot_addr: usize) -> u8 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let recovered = ptr_shadow::load_export_parent_recovered(slot_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_export_parent_recovered slot=0x{:x} -> {}",
            slot_addr, recovered
        );
    }
    recovered
}

#[no_mangle]
pub extern "C" fn __rz_shadow_load_export_parent_recovered_for_ptr(
    slot_addr: usize,
    ptr_addr: usize,
) -> u8 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_load);
    let _g = RzRuntimeGuard::enter();
    let recovered = ptr_shadow::load_export_parent_recovered_for_ptr_value(slot_addr, ptr_addr);
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] load_export_parent_recovered_for_ptr slot=0x{:x} ptr=0x{:x} -> {}",
            slot_addr, ptr_addr, recovered
        );
    }
    recovered
}

#[no_mangle]
pub extern "C" fn __rz_shadow_kill_range(slot_addr: usize, size: usize) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_kill_range);
    let _g = RzRuntimeGuard::enter();
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] kill slot=0x{:x} size={}",
            slot_addr, size
        );
    }
    ptr_shadow::kill_range(slot_addr, size);
}

#[no_mangle]
pub extern "C" fn __rz_tag_kill(tag: u64) {
    if tag == 0 {
        return;
    }
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, tag_kill);
    let _g = RzRuntimeGuard::enter();
    let release_start = rz_profile_start!(profile);
    let released = tag_store::release_local_holder(tag);
    rz_profile_add_elapsed_field!(profile, tag_kill_release_ns, release_start);
    if released {
        let escape_check_start = rz_profile_start!(profile);
        let escaped = tag_store::active_tag_escaped(tag);
        rz_profile_add_elapsed_field!(profile, tag_kill_escape_check_ns, escape_check_start);
        if !escaped {
            let alias_on_tag_killed_start = rz_profile_start!(profile);
            active_alias_model().on_tag_killed(tag);
            rz_profile_add_elapsed_field!(
                profile,
                tag_kill_alias_on_tag_killed_ns,
                alias_on_tag_killed_start
            );
        }
    }
}

#[no_mangle]
pub extern "C" fn __rz_tag_retain(tag: u64) {
    if tag == 0 {
        return;
    }
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, tag_retain);
    let _g = RzRuntimeGuard::enter();
    tag_store::retain_local_holder(tag);
}

#[no_mangle]
pub extern "C" fn __rz_shadow_copy_slot(dst_slot_addr: usize, src_slot_addr: usize) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_copy_slot);
    let _g = RzRuntimeGuard::enter();
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] copy dst=0x{:x} src=0x{:x}",
            dst_slot_addr, src_slot_addr
        );
    }
    let (tag, ref_ancestor, export_parent, export_parent_recovered) =
        current_slot_shadow(src_slot_addr).unwrap_or((0, 0, 0, 0));
    let (tag, ref_ancestor, export_parent, export_parent_recovered) =
        sanitize_shadow_entry_for_slot_value(
            dst_slot_addr,
            tag,
            ref_ancestor,
            export_parent,
            export_parent_recovered,
        );
    ptr_shadow::store_ptr(
        dst_slot_addr,
        tag,
        ref_ancestor,
        export_parent,
        export_parent_recovered,
    );
}

#[no_mangle]
pub extern "C" fn __rz_shadow_copy_range(dst_addr: usize, src_addr: usize, size: usize) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, shadow_copy_range);
    let _g = RzRuntimeGuard::enter();
    if rz_trace_ptr_shadow_enabled() {
        eprintln!(
            "[rusteze-runtime][ptr-shadow] copy_range dst=0x{:x} src=0x{:x} size={}",
            dst_addr, src_addr, size
        );
    }
    ptr_shadow::copy_range(dst_addr, src_addr, size);
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
        Self {
            seq: 0,
            base: 0,
            size: 0,
            live: 0,
        }
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
        ALLOC_LOG[idx] = AllocLogEntry {
            seq,
            base: base_addr,
            size,
            live,
        };
    }
    // Also keep a heap-only ring buffer to avoid stack noise.
    if (live & 0x2) == 0 {
        let hidx = ALLOC_LOG_HEAP_IDX.fetch_add(1, Ordering::Relaxed) % ALLOC_LOG_HEAP_SIZE;
        let hseq = ALLOC_LOG_HEAP_SEQ.fetch_add(1, Ordering::Relaxed);
        unsafe {
            ALLOC_LOG_HEAP[hidx] = AllocLogEntry {
                seq: hseq,
                base: base_addr,
                size,
                live,
            };
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
            Self {
                buf: [0u8; 256],
                len: 0,
            }
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
            unsafe { &raw const ALLOC_LOG_HEAP as *const [AllocLogEntry; ALLOC_LOG_HEAP_SIZE] },
        )
    } else {
        (
            ALLOC_LOG_IDX.load(Ordering::Relaxed),
            ALLOC_LOG_SIZE,
            unsafe { &raw const ALLOC_LOG as *const [AllocLogEntry; ALLOC_LOG_SIZE] },
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
            Self {
                buf: [0u8; 256],
                len: 0,
            }
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

#[no_mangle]
#[cfg(feature = "runtime_hook_profile")]
pub extern "C" fn __rz_reset_hook_profile() {
    let Some(p) = RZ_HOOK_PROFILE.get() else {
        return;
    };
    p.write_calls.store(0, Ordering::Relaxed);
    p.write_total_ns.store(0, Ordering::Relaxed);
    p.write_tag_lookup_ns.store(0, Ordering::Relaxed);
    p.write_alias_check_ns.store(0, Ordering::Relaxed);
    p.write_alloc_lookup_ns.store(0, Ordering::Relaxed);
    p.read_calls.store(0, Ordering::Relaxed);
    p.read_total_ns.store(0, Ordering::Relaxed);
    p.read_tag_lookup_ns.store(0, Ordering::Relaxed);
    p.read_alias_check_ns.store(0, Ordering::Relaxed);
    p.read_alloc_lookup_ns.store(0, Ordering::Relaxed);
    p.ref_create_calls.store(0, Ordering::Relaxed);
    p.ref_create_total_ns.store(0, Ordering::Relaxed);
    p.ref_create_validate_ns.store(0, Ordering::Relaxed);
    p.ref_create_alloc_snapshot_ns.store(0, Ordering::Relaxed);
    p.ref_create_insert_ns.store(0, Ordering::Relaxed);
    p.ref_create_tag_store_insert_ns.store(0, Ordering::Relaxed);
    p.ref_create_alias_on_tag_created_ns
        .store(0, Ordering::Relaxed);
    p.raw_create_calls.store(0, Ordering::Relaxed);
    p.raw_create_total_ns.store(0, Ordering::Relaxed);
    p.ptr_use_calls.store(0, Ordering::Relaxed);
    p.ptr_use_total_ns.store(0, Ordering::Relaxed);
    p.record_alloc_calls.store(0, Ordering::Relaxed);
    p.record_alloc_total_ns.store(0, Ordering::Relaxed);
    p.shadow_store_calls.store(0, Ordering::Relaxed);
    p.shadow_store_total_ns.store(0, Ordering::Relaxed);
    p.shadow_load_calls.store(0, Ordering::Relaxed);
    p.shadow_load_total_ns.store(0, Ordering::Relaxed);
    p.shadow_kill_range_calls.store(0, Ordering::Relaxed);
    p.shadow_kill_range_total_ns.store(0, Ordering::Relaxed);
    p.shadow_copy_slot_calls.store(0, Ordering::Relaxed);
    p.shadow_copy_slot_total_ns.store(0, Ordering::Relaxed);
    p.shadow_copy_range_calls.store(0, Ordering::Relaxed);
    p.shadow_copy_range_total_ns.store(0, Ordering::Relaxed);
    p.tag_retain_calls.store(0, Ordering::Relaxed);
    p.tag_retain_total_ns.store(0, Ordering::Relaxed);
    p.tag_kill_calls.store(0, Ordering::Relaxed);
    p.tag_kill_total_ns.store(0, Ordering::Relaxed);
    p.tag_kill_release_ns.store(0, Ordering::Relaxed);
    p.tag_kill_escape_check_ns.store(0, Ordering::Relaxed);
    p.tag_kill_alias_on_tag_killed_ns
        .store(0, Ordering::Relaxed);
    p.call_scope_calls.store(0, Ordering::Relaxed);
    p.call_scope_total_ns.store(0, Ordering::Relaxed);
    p.call_arg_push_calls.store(0, Ordering::Relaxed);
    p.call_arg_push_total_ns.store(0, Ordering::Relaxed);
    p.call_arg_zero_skipped.store(0, Ordering::Relaxed);
    p.call_arg_take_calls.store(0, Ordering::Relaxed);
    p.call_arg_take_total_ns.store(0, Ordering::Relaxed);
    p.call_arg_leaf_calls.store(0, Ordering::Relaxed);
    p.call_arg_leaf_total_ns.store(0, Ordering::Relaxed);
    p.mut_arg_ret_push_calls.store(0, Ordering::Relaxed);
    p.mut_arg_ret_push_total_ns.store(0, Ordering::Relaxed);
    p.mut_arg_ret_take_calls.store(0, Ordering::Relaxed);
    p.mut_arg_ret_take_total_ns.store(0, Ordering::Relaxed);
    p.ret_push_calls.store(0, Ordering::Relaxed);
    p.ret_push_total_ns.store(0, Ordering::Relaxed);
    p.ret_take_calls.store(0, Ordering::Relaxed);
    p.ret_take_total_ns.store(0, Ordering::Relaxed);
    p.fn_enter_calls.store(0, Ordering::Relaxed);
    p.fn_enter_total_ns.store(0, Ordering::Relaxed);
    p.fn_exit_calls.store(0, Ordering::Relaxed);
    p.fn_exit_total_ns.store(0, Ordering::Relaxed);
}

#[no_mangle]
#[cfg(not(feature = "runtime_hook_profile"))]
pub extern "C" fn __rz_reset_hook_profile() {}

#[no_mangle]
#[cfg(feature = "runtime_hook_profile")]
pub extern "C" fn __rz_dump_hook_profile() {
    let _runtime_guard = RzRuntimeGuard::enter();
    if !rz_profile_hooks_enabled() {
        eprintln!("[rusteze-runtime] hook profile: disabled (set RZ_PROFILE_HOOKS=1)");
        return;
    }
    let p = rz_hook_profile();
    let write_calls = p.write_calls.load(Ordering::Relaxed);
    let read_calls = p.read_calls.load(Ordering::Relaxed);
    let write_total_ns = p.write_total_ns.load(Ordering::Relaxed);
    let read_total_ns = p.read_total_ns.load(Ordering::Relaxed);
    let write_tag_ns = p.write_tag_lookup_ns.load(Ordering::Relaxed);
    let read_tag_ns = p.read_tag_lookup_ns.load(Ordering::Relaxed);
    let write_alias_ns = p.write_alias_check_ns.load(Ordering::Relaxed);
    let read_alias_ns = p.read_alias_check_ns.load(Ordering::Relaxed);
    let write_alloc_ns = p.write_alloc_lookup_ns.load(Ordering::Relaxed);
    let read_alloc_ns = p.read_alloc_lookup_ns.load(Ordering::Relaxed);
    let ref_create_calls = p.ref_create_calls.load(Ordering::Relaxed);
    let ref_create_total_ns = p.ref_create_total_ns.load(Ordering::Relaxed);
    let ref_create_validate_ns = p.ref_create_validate_ns.load(Ordering::Relaxed);
    let ref_create_alloc_snapshot_ns = p.ref_create_alloc_snapshot_ns.load(Ordering::Relaxed);
    let ref_create_insert_ns = p.ref_create_insert_ns.load(Ordering::Relaxed);
    let ref_create_tag_store_insert_ns = p.ref_create_tag_store_insert_ns.load(Ordering::Relaxed);
    let ref_create_alias_on_tag_created_ns =
        p.ref_create_alias_on_tag_created_ns.load(Ordering::Relaxed);
    let raw_create_calls = p.raw_create_calls.load(Ordering::Relaxed);
    let raw_create_total_ns = p.raw_create_total_ns.load(Ordering::Relaxed);
    let ptr_use_calls = p.ptr_use_calls.load(Ordering::Relaxed);
    let ptr_use_total_ns = p.ptr_use_total_ns.load(Ordering::Relaxed);
    let record_alloc_calls = p.record_alloc_calls.load(Ordering::Relaxed);
    let record_alloc_total_ns = p.record_alloc_total_ns.load(Ordering::Relaxed);
    let shadow_store_calls = p.shadow_store_calls.load(Ordering::Relaxed);
    let shadow_store_total_ns = p.shadow_store_total_ns.load(Ordering::Relaxed);
    let shadow_load_calls = p.shadow_load_calls.load(Ordering::Relaxed);
    let shadow_load_total_ns = p.shadow_load_total_ns.load(Ordering::Relaxed);
    let shadow_kill_range_calls = p.shadow_kill_range_calls.load(Ordering::Relaxed);
    let shadow_kill_range_total_ns = p.shadow_kill_range_total_ns.load(Ordering::Relaxed);
    let shadow_copy_slot_calls = p.shadow_copy_slot_calls.load(Ordering::Relaxed);
    let shadow_copy_slot_total_ns = p.shadow_copy_slot_total_ns.load(Ordering::Relaxed);
    let shadow_copy_range_calls = p.shadow_copy_range_calls.load(Ordering::Relaxed);
    let shadow_copy_range_total_ns = p.shadow_copy_range_total_ns.load(Ordering::Relaxed);
    let tag_retain_calls = p.tag_retain_calls.load(Ordering::Relaxed);
    let tag_retain_total_ns = p.tag_retain_total_ns.load(Ordering::Relaxed);
    let tag_kill_calls = p.tag_kill_calls.load(Ordering::Relaxed);
    let tag_kill_total_ns = p.tag_kill_total_ns.load(Ordering::Relaxed);
    let tag_kill_release_ns = p.tag_kill_release_ns.load(Ordering::Relaxed);
    let tag_kill_escape_check_ns = p.tag_kill_escape_check_ns.load(Ordering::Relaxed);
    let tag_kill_alias_on_tag_killed_ns = p.tag_kill_alias_on_tag_killed_ns.load(Ordering::Relaxed);
    let call_scope_calls = p.call_scope_calls.load(Ordering::Relaxed);
    let call_scope_total_ns = p.call_scope_total_ns.load(Ordering::Relaxed);
    let call_arg_push_calls = p.call_arg_push_calls.load(Ordering::Relaxed);
    let call_arg_push_total_ns = p.call_arg_push_total_ns.load(Ordering::Relaxed);
    let call_arg_zero_skipped = p.call_arg_zero_skipped.load(Ordering::Relaxed);
    let call_arg_take_calls = p.call_arg_take_calls.load(Ordering::Relaxed);
    let call_arg_take_total_ns = p.call_arg_take_total_ns.load(Ordering::Relaxed);
    let call_arg_leaf_calls = p.call_arg_leaf_calls.load(Ordering::Relaxed);
    let call_arg_leaf_total_ns = p.call_arg_leaf_total_ns.load(Ordering::Relaxed);
    let mut_arg_ret_push_calls = p.mut_arg_ret_push_calls.load(Ordering::Relaxed);
    let mut_arg_ret_push_total_ns = p.mut_arg_ret_push_total_ns.load(Ordering::Relaxed);
    let mut_arg_ret_take_calls = p.mut_arg_ret_take_calls.load(Ordering::Relaxed);
    let mut_arg_ret_take_total_ns = p.mut_arg_ret_take_total_ns.load(Ordering::Relaxed);
    let ret_push_calls = p.ret_push_calls.load(Ordering::Relaxed);
    let ret_push_total_ns = p.ret_push_total_ns.load(Ordering::Relaxed);
    let ret_take_calls = p.ret_take_calls.load(Ordering::Relaxed);
    let ret_take_total_ns = p.ret_take_total_ns.load(Ordering::Relaxed);
    let fn_enter_calls = p.fn_enter_calls.load(Ordering::Relaxed);
    let fn_enter_total_ns = p.fn_enter_total_ns.load(Ordering::Relaxed);
    let fn_exit_calls = p.fn_exit_calls.load(Ordering::Relaxed);
    let fn_exit_total_ns = p.fn_exit_total_ns.load(Ordering::Relaxed);

    let write_avg_ns = if write_calls == 0 {
        0.0
    } else {
        write_total_ns as f64 / write_calls as f64
    };
    let read_avg_ns = if read_calls == 0 {
        0.0
    } else {
        read_total_ns as f64 / read_calls as f64
    };
    let (alloc_entries, live_alloc_entries) = {
        let amap = allocs().lock().unwrap();
        let live = amap.values().filter(|m| m.live).count();
        (amap.len(), live)
    };
    let tag_entries = tag_store::len();
    let historical_live_tag_entries = tag_store::historical_live_len();
    let invalidated_tag_entries = tag_store::invalidated_len();
    let dead_tag_entries = tag_store::dead_len();
    let tag_history_stats = tag_pruning::stats();
    let call_arg_entries = call_arg_tags().lock().unwrap().len();
    let ret_tag_entries = ret_tags().lock().unwrap().len();

    eprintln!("[rusteze-runtime] hook profile (ns):");
    eprintln!(
        "  write: calls={} total={} avg_per_call={:.1} tag_lookup={} alias_check={} alloc_lookup={}",
        write_calls, write_total_ns, write_avg_ns, write_tag_ns, write_alias_ns, write_alloc_ns
    );
    eprintln!(
        "  read:  calls={} total={} avg_per_call={:.1} tag_lookup={} alias_check={} alloc_lookup={}",
        read_calls, read_total_ns, read_avg_ns, read_tag_ns, read_alias_ns, read_alloc_ns
    );
    eprintln!(
        "  ref_create:  calls={} total={} avg_per_call={:.1} validate={} alloc_snapshot={} insert={} tag_store_insert={} alias_on_tag_created={}",
        ref_create_calls,
        ref_create_total_ns,
        if ref_create_calls == 0 {
            0.0
        } else {
            ref_create_total_ns as f64 / ref_create_calls as f64
        },
        ref_create_validate_ns,
        ref_create_alloc_snapshot_ns,
        ref_create_insert_ns,
        ref_create_tag_store_insert_ns,
        ref_create_alias_on_tag_created_ns
    );
    eprintln!(
        "  raw_create:  calls={} total={} avg_per_call={:.1}",
        raw_create_calls,
        raw_create_total_ns,
        if raw_create_calls == 0 {
            0.0
        } else {
            raw_create_total_ns as f64 / raw_create_calls as f64
        }
    );
    eprintln!(
        "  ptr_use:     calls={} total={} avg_per_call={:.1}",
        ptr_use_calls,
        ptr_use_total_ns,
        if ptr_use_calls == 0 {
            0.0
        } else {
            ptr_use_total_ns as f64 / ptr_use_calls as f64
        }
    );
    eprintln!(
        "  record_alloc:calls={} total={} avg_per_call={:.1}",
        record_alloc_calls,
        record_alloc_total_ns,
        if record_alloc_calls == 0 {
            0.0
        } else {
            record_alloc_total_ns as f64 / record_alloc_calls as f64
        }
    );
    eprintln!(
        "  shadow_store:      calls={} total={} avg_per_call={:.1}",
        shadow_store_calls,
        shadow_store_total_ns,
        if shadow_store_calls == 0 {
            0.0
        } else {
            shadow_store_total_ns as f64 / shadow_store_calls as f64
        }
    );
    eprintln!(
        "  shadow_load:       calls={} total={} avg_per_call={:.1}",
        shadow_load_calls,
        shadow_load_total_ns,
        if shadow_load_calls == 0 {
            0.0
        } else {
            shadow_load_total_ns as f64 / shadow_load_calls as f64
        }
    );
    eprintln!(
        "  shadow_kill_range: calls={} total={} avg_per_call={:.1}",
        shadow_kill_range_calls,
        shadow_kill_range_total_ns,
        if shadow_kill_range_calls == 0 {
            0.0
        } else {
            shadow_kill_range_total_ns as f64 / shadow_kill_range_calls as f64
        }
    );
    eprintln!(
        "  shadow_copy_slot:  calls={} total={} avg_per_call={:.1}",
        shadow_copy_slot_calls,
        shadow_copy_slot_total_ns,
        if shadow_copy_slot_calls == 0 {
            0.0
        } else {
            shadow_copy_slot_total_ns as f64 / shadow_copy_slot_calls as f64
        }
    );
    eprintln!(
        "  shadow_copy_range: calls={} total={} avg_per_call={:.1}",
        shadow_copy_range_calls,
        shadow_copy_range_total_ns,
        if shadow_copy_range_calls == 0 {
            0.0
        } else {
            shadow_copy_range_total_ns as f64 / shadow_copy_range_calls as f64
        }
    );
    eprintln!(
        "  tag_retain:        calls={} total={} avg_per_call={:.1}",
        tag_retain_calls,
        tag_retain_total_ns,
        if tag_retain_calls == 0 {
            0.0
        } else {
            tag_retain_total_ns as f64 / tag_retain_calls as f64
        }
    );
    eprintln!(
        "  tag_kill:          calls={} total={} avg_per_call={:.1} release={} escape_check={} alias_on_tag_killed={}",
        tag_kill_calls,
        tag_kill_total_ns,
        if tag_kill_calls == 0 {
            0.0
        } else {
            tag_kill_total_ns as f64 / tag_kill_calls as f64
        },
        tag_kill_release_ns,
        tag_kill_escape_check_ns,
        tag_kill_alias_on_tag_killed_ns
    );
    eprintln!(
        "  call_scope:        calls={} total={} avg_per_call={:.1}",
        call_scope_calls,
        call_scope_total_ns,
        if call_scope_calls == 0 {
            0.0
        } else {
            call_scope_total_ns as f64 / call_scope_calls as f64
        }
    );
    eprintln!(
        "  call_arg_push:     calls={} total={} avg_per_call={:.1} zero_skipped={}",
        call_arg_push_calls,
        call_arg_push_total_ns,
        if call_arg_push_calls == 0 {
            0.0
        } else {
            call_arg_push_total_ns as f64 / call_arg_push_calls as f64
        },
        call_arg_zero_skipped
    );
    eprintln!(
        "  call_arg_take:     calls={} total={} avg_per_call={:.1}",
        call_arg_take_calls,
        call_arg_take_total_ns,
        if call_arg_take_calls == 0 {
            0.0
        } else {
            call_arg_take_total_ns as f64 / call_arg_take_calls as f64
        }
    );
    eprintln!(
        "  call_arg_leaf:     calls={} total={} avg_per_call={:.1}",
        call_arg_leaf_calls,
        call_arg_leaf_total_ns,
        if call_arg_leaf_calls == 0 {
            0.0
        } else {
            call_arg_leaf_total_ns as f64 / call_arg_leaf_calls as f64
        }
    );
    eprintln!(
        "  mut_arg_ret_push:  calls={} total={} avg_per_call={:.1}",
        mut_arg_ret_push_calls,
        mut_arg_ret_push_total_ns,
        if mut_arg_ret_push_calls == 0 {
            0.0
        } else {
            mut_arg_ret_push_total_ns as f64 / mut_arg_ret_push_calls as f64
        }
    );
    eprintln!(
        "  mut_arg_ret_take:  calls={} total={} avg_per_call={:.1}",
        mut_arg_ret_take_calls,
        mut_arg_ret_take_total_ns,
        if mut_arg_ret_take_calls == 0 {
            0.0
        } else {
            mut_arg_ret_take_total_ns as f64 / mut_arg_ret_take_calls as f64
        }
    );
    eprintln!(
        "  ret_push:          calls={} total={} avg_per_call={:.1}",
        ret_push_calls,
        ret_push_total_ns,
        if ret_push_calls == 0 {
            0.0
        } else {
            ret_push_total_ns as f64 / ret_push_calls as f64
        }
    );
    eprintln!(
        "  ret_take:          calls={} total={} avg_per_call={:.1}",
        ret_take_calls,
        ret_take_total_ns,
        if ret_take_calls == 0 {
            0.0
        } else {
            ret_take_total_ns as f64 / ret_take_calls as f64
        }
    );
    eprintln!(
        "  fn_enter:          calls={} total={} avg_per_call={:.1}",
        fn_enter_calls,
        fn_enter_total_ns,
        if fn_enter_calls == 0 {
            0.0
        } else {
            fn_enter_total_ns as f64 / fn_enter_calls as f64
        }
    );
    eprintln!(
        "  fn_exit:           calls={} total={} avg_per_call={:.1}",
        fn_exit_calls,
        fn_exit_total_ns,
        if fn_exit_calls == 0 {
            0.0
        } else {
            fn_exit_total_ns as f64 / fn_exit_calls as f64
        }
    );
    eprintln!(
        "  state: alloc_entries={} live_alloc_entries={} tag_entries={} historical_live_tag_entries={} invalidated_tag_entries={} dead_tag_entries={} call_arg_entries={} ret_tag_entries={}",
        alloc_entries, live_alloc_entries, tag_entries, historical_live_tag_entries, invalidated_tag_entries, dead_tag_entries, call_arg_entries, ret_tag_entries
    );
    eprintln!(
        "  tag_pruning: active_epoch_buckets={} active_tag_entries={} historical_live_tag_entries={} invalidated_tag_entries={} dead_epoch_buckets={} dead_tag_entries={} shadowed_old_live_tag_candidates={}",
        tag_history_stats.active_epoch_buckets,
        tag_history_stats.active_tag_entries,
        tag_history_stats.historical_live_tag_entries,
        tag_history_stats.invalidated_tag_entries,
        tag_history_stats.dead_epoch_buckets,
        tag_history_stats.dead_tag_entries,
        tag_history_stats.shadowed_old_live_tag_candidates
    );
}

#[no_mangle]
#[cfg(not(feature = "runtime_hook_profile"))]
pub extern "C" fn __rz_dump_hook_profile() {
    eprintln!("[rusteze-runtime] hook profile: compiled out (enable feature runtime_hook_profile)");
}

/// Record/validate a write through a tracked pointer tag.
/// Best-effort checks:
///  - tag must exist
///  - fast path uses tag-cached origin bounds + exact-base alloc lookup
///  - slow path falls back to range lookup when cache is missing/invalid
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
pub fn __rz_ptr_write(
    tag: u64,
    addr: usize,
    size: usize,
    align_req: usize,
    access_alias_exempt: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, write);

    let tag = if tag == 0 {
        if rz_allow_untagged() {
            return;
        }
        if rz_tag0_as_root() {
            __record_raw_ptr_creation(addr, 1, 0, 0, 0, align_req)
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
    let tag_lookup_start = rz_profile_start!(profile);
    let Some(mut tmeta) = tag_lookup_cache::get_cached(tag, || tag_store::get(tag)) else {
        let msg = append_location_if_enabled(
            format!("WRITE unknown tag={tag} addr=0x{addr:x} size={size}"),
            "RZ_LOG_LOC",
        );
        rz_violation("UNKNOWN_TAG", msg);
        return;
    };
    if rz_has_exposed_provenance_root(tag, &tmeta) {
        if rz_strict_provenance_enabled() {
            let msg = append_location_if_enabled(
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\nreason=NO_PROVENANCE_ACCESS kind={:?} parent={} pointee=0x{:x}",
                    tmeta.kind, tmeta.parent, tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation("WILD_POINTER", msg);
            return;
        }
        if tmeta.parent == 0 {
            let msg = append_location_if_enabled(
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\nreason=NO_PROVENANCE_ACCESS kind={:?} parent={} pointee=0x{:x}",
                    tmeta.kind, tmeta.parent, tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation("WILD_POINTER", msg);
            return;
        }
        if let Err(reason) =
            validate_exposed_live_alloc_access(tmeta.pointee_addr, tmeta.alloc_epoch, addr, size)
        {
            let msg = append_location_if_enabled(
                format!(
                    "WRITE via tag={tag} addr=0x{addr:x} size={size}\nreason={} kind={:?} parent={} pointee=0x{:x}",
                    reason.label(),
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation(reason.violation_kind(), msg);
            return;
        }
    }
    tmeta.alias_exempt |=
        access_alias_exempt != 0 || tag_alias_exempt_via_bounded_ancestor(tag, addr, size);
    let (addr, size) = normalize_const_end_ref_access_addr(&tmeta, addr, size);
    let guaranteed_align = tmeta
        .align_req
        .max(rz_promised_alignment_for_addr(addr, tmeta.alloc_epoch));
    let align_req = if align_req != 0 {
        align_req
    } else {
        tmeta.align_req
    };
    rz_check_alignment(
        "WRITE",
        tag,
        addr,
        size,
        guaranteed_align,
        align_req,
        Some(&tmeta),
    );
    let sb_tag_opt = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        match active_alias_model().name() {
            // Tree Borrows tracks raws as first-class nodes in the tree.
            // Rewriting them to a reference ancestor skips state transitions
            // that should happen on the raw itself.
            "tb_lite" => {
                if (tmeta.lineage_hint & LINEAGE_HINT_TB_RAW_REUSE_PARENT_FAMILY) != 0 {
                    let tmap = tags().lock().unwrap();
                    active_alias_model()
                        .find_ref_ancestor_tag(&tmap, tag)
                        .or(Some(tag))
                } else {
                    Some(tag)
                }
            }
            "sb_lite" => {
                let tmap = tags().lock().unwrap();
                active_alias_model()
                    .find_ref_ancestor_tag(&tmap, tag)
                    .or(Some(tag))
            }
            _ => {
                let tmap = tags().lock().unwrap();
                active_alias_model().find_ref_ancestor_tag(&tmap, tag)
            }
        }
    } else {
        Some(tag)
    };
    rz_profile_add_opt_field!(profile, write_tag_lookup_ns, tag_lookup_start);

    if let Some(sb_tag) = sb_tag_opt {
        let alias_check_start = rz_profile_start!(profile);
        let alias_violation = active_alias_model().check_access(
            sb_tag,
            tag,
            &tmeta,
            addr,
            size,
            AliasAccessKind::Write,
        );
        rz_profile_add_opt_field!(profile, write_alias_check_ns, alias_check_start);
        if let Some(msg) = alias_violation {
            rz_violation(
                active_alias_model().violation_kind(),
                append_location_if_enabled(msg, "RZ_LOG_LOC"),
            );
            return;
        }
    }

    // Fast path: tag-cached origin bounds + exact-base alloc lookup.
    // Slow path falls back to range lookup only when cache is missing/invalid.
    let alloc_lookup_start = rz_profile_start!(profile);
    let trace_enabled = rz_log_enabled(LogLevel::Trace);
    let cached_origin_oob = tag_origin_oob_cached(&tmeta, addr, size);
    let origin_base_alloc = alloc_from_origin_base(&tmeta);
    let mut alloc_opt: Option<(usize, AllocMeta)> = None;

    if !cached_origin_oob {
        if tag_origin_contains_access(&tmeta, addr, size) {
            alloc_opt = origin_base_alloc;
        }

        if alloc_opt.is_none() {
            alloc_opt = if trace_enabled {
                let amap = allocs().lock().unwrap();
                rz_trace!(
                    "[rusteze-runtime] WRITE lookup (slow): addr=0x{:x} size={} tag={}",
                    addr,
                    size,
                    tag
                );
                let mut shown = 0usize;
                for (b, m) in amap.range(..=addr).rev() {
                    if shown >= 8 {
                        break;
                    }
                    let end = b.saturating_add(m.size);
                    rz_trace!(
                        "  cand base=0x{:x} size={} live={} epoch={} end=0x{:x}",
                        b,
                        m.size,
                        m.live,
                        m.epoch,
                        end
                    );
                    shown += 1;
                }
                find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
            } else {
                live_alloc_cache::lookup_containing(addr).or_else(|| {
                    let amap = allocs().lock().unwrap();
                    find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
                })
            };
        }
    } else if origin_base_alloc.is_none() {
        // Cached origin exists but exact-base entry disappeared; revalidate via slow path.
        alloc_opt = live_alloc_cache::lookup_containing(addr).or_else(|| {
            let amap = allocs().lock().unwrap();
            find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
        });
    }

    if let Some((base, ameta)) = alloc_opt {
        refresh_tag_origin_cache(tag, &mut tmeta, base, &ameta);
    }
    rz_profile_add_opt_field!(profile, write_alloc_lookup_ns, alloc_lookup_start);

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
        // If we can prove (via tag-cached origin + epoch snapshot) that this pointer was derived
        // from a particular allocation, classify this as OUT_OF_BOUNDS rather than WILD_POINTER.
        let cached_origin_alloc = if tmeta.origin_known {
            origin_base_alloc
        } else {
            None
        };
        let fallback_origin_alloc = if cached_origin_alloc.is_none() {
            let amap = allocs().lock().unwrap();
            let tmap = tags().lock().unwrap();
            origin_alloc_for_tag(&tmap, &amap, tag).map(|(base, meta)| (base, *meta))
        } else {
            None
        };
        if let Some((obase, ometa)) = cached_origin_alloc.or(fallback_origin_alloc) {
            if ometa.size != 0 && size != 0 {
                let access_end = addr.saturating_add(size);
                let alloc_end = obase.saturating_add(ometa.size);

                // If the access overlaps beyond the end of the origin allocation, it's OOB.
                if addr >= obase && access_end > alloc_end {
                    if rz_allow_stack_ref_oob_noise(&tmeta, &ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_ref_root_boundary_oob_noise(&tmeta, &ometa, obase, addr, size)
                    {
                        return;
                    }
                    if rz_allow_stack_raw_root_oob_noise(&tmeta, &ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_raw_nonroot_boundary_oob_noise(
                        &tmeta, &ometa, obase, addr, size,
                    ) {
                        return;
                    }
                    if rz_allow_projected_raw_stack_slot_oob_noise(
                        &tmeta, &ometa, obase, addr, size,
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
                    rz_violation("OUT_OF_BOUNDS", msg);
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
                rz_violation("WRITE_TO_READONLY_STATIC", msg);
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
        rz_violation("WILD_POINTER", msg);
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
        // `slot_reused` is true when we can prove the tag was minted against an older
        // allocation instance (gap >= 2). When the tag's alloc_epoch is unknown (0) we
        // conservatively treat stack accesses as ambiguous and also suppress, matching
        // the previous unconditional stack-skip behavior for that specific case.
        let slot_reused = tmeta.alloc_epoch == 0 || ameta.epoch > tmeta.alloc_epoch + 1;
        if ameta.is_stack && slot_reused && tmeta.parent != 0 && tmeta.pointee_addr == base {
            return;
        }
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
            && (tmeta.alloc_epoch == 0 || !tmeta.origin_known)
            && (!ameta.is_stack || slot_reused)
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
        rz_violation("USE_AFTER_DEAD", msg);
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
        rz_violation("STALE_POINTER_EPOCH_MISMATCH", msg);
        return;
    }

    // Bounds check against wide-pointer metadata (slice/str) if available.
    if bounds_len_has_explicit_byte_bounds(tmeta.bounds_len) && size != 0 {
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
                rz_violation("OUT_OF_BOUNDS", msg);
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
            rz_violation("OUT_OF_BOUNDS", msg);
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
                rz_violation("OUT_OF_BOUNDS", msg);
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
            rz_violation("OUT_OF_BOUNDS", msg);
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
pub fn __rz_ptr_write_allow_untagged(
    tag: u64,
    addr: usize,
    size: usize,
    align_req: usize,
    access_alias_exempt: u8,
) {
    if tag == 0 {
        return;
    }
    let _sb = SbSuppressGuard::enter();
    let _relax = RelaxEpochGuard::enter();
    __rz_ptr_write(tag, addr, size, align_req, access_alias_exempt);
}

/// Record a direct write to a stack slot/root local.
/// This is not a normal tagged pointer dereference. Model it as a fresh unique child borrow
/// rooted at the current local-anchor family, then perform the write through that fresh tag.
#[no_mangle]
#[track_caller]
pub fn __rz_local_write_allow_untagged(tag: u64, addr: usize, size: usize) {
    if tag == 0 || size == 0 {
        return;
    }
    let parent_tag = match tag_store::get(tag).map(|m| m.kind) {
        Some(PtrKind::RefShared | PtrKind::RawConst) => 0,
        _ => tag,
    };
    // This is a concrete write to a real stack slot, not a dereference through the parent's
    // pointee type. Use the slot's runtime address alignment instead of inheriting the parent's
    // stronger alignment guarantee, otherwise short-lived scalar locals can spuriously reuse an
    // outer container's alignment (e.g. `u8` loop items inheriting `IntoIter`'s `align=8`).
    let slot_align = rz_addr_alignment(addr);
    let write_tag = __record_ref_creation(addr, 1, parent_tag, 0, size, slot_align);
    let _relax = RelaxEpochGuard::enter();
    __rz_ptr_write(write_tag, addr, size, slot_align, 0);
}

/// Record/validate a read through a tracked pointer tag.
/// Best-effort checks:
///  - tag must exist
///  - fast path uses tag-cached origin bounds + exact-base alloc lookup
///  - slow path falls back to range lookup when cache is missing/invalid
///  - if both alloc and tag have epochs, they must match
#[no_mangle]
pub fn __rz_ptr_read(
    tag: u64,
    addr: usize,
    size: usize,
    align_req: usize,
    access_alias_exempt: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, read);

    let tag = if tag == 0 {
        if rz_allow_untagged() {
            return;
        }
        if rz_tag0_as_root() {
            __record_raw_ptr_creation(addr, 0, 0, 0, 0, align_req)
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
    let tag_lookup_start = rz_profile_start!(profile);
    let Some(mut tmeta) = tag_lookup_cache::get_cached(tag, || tag_store::get(tag)) else {
        let msg = append_location_if_enabled(
            format!("READ unknown tag={tag} addr=0x{addr:x} size={size}"),
            "RZ_LOG_LOC",
        );
        rz_violation("UNKNOWN_TAG", msg);
        return;
    };
    if rz_has_exposed_provenance_root(tag, &tmeta) {
        if rz_strict_provenance_enabled() {
            let msg = append_location_if_enabled(
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\nreason=NO_PROVENANCE_ACCESS kind={:?} parent={} pointee=0x{:x}",
                    tmeta.kind, tmeta.parent, tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation("WILD_POINTER", msg);
            return;
        }
        if tmeta.parent == 0 {
            let msg = append_location_if_enabled(
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\nreason=NO_PROVENANCE_ACCESS kind={:?} parent={} pointee=0x{:x}",
                    tmeta.kind, tmeta.parent, tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation("WILD_POINTER", msg);
            return;
        }
        if let Err(reason) =
            validate_exposed_live_alloc_access(tmeta.pointee_addr, tmeta.alloc_epoch, addr, size)
        {
            let msg = append_location_if_enabled(
                format!(
                    "READ via tag={tag} addr=0x{addr:x} size={size}\nreason={} kind={:?} parent={} pointee=0x{:x}",
                    reason.label(),
                    tmeta.kind,
                    tmeta.parent,
                    tmeta.pointee_addr
                ),
                "RZ_LOG_LOC",
            );
            rz_violation(reason.violation_kind(), msg);
            return;
        }
    }
    tmeta.alias_exempt |=
        access_alias_exempt != 0 || tag_alias_exempt_via_bounded_ancestor(tag, addr, size);
    let (addr, size) = normalize_const_end_ref_access_addr(&tmeta, addr, size);
    let guaranteed_align = tmeta
        .align_req
        .max(rz_promised_alignment_for_addr(addr, tmeta.alloc_epoch));
    let align_req = if align_req != 0 {
        align_req
    } else {
        tmeta.align_req
    };
    rz_check_alignment(
        "READ",
        tag,
        addr,
        size,
        guaranteed_align,
        align_req,
        Some(&tmeta),
    );
    let sb_tag_opt = if matches!(tmeta.kind, PtrKind::RawConst | PtrKind::RawMut) {
        match active_alias_model().name() {
            // Tree Borrows tracks raws as first-class nodes in the tree.
            // Rewriting them to a reference ancestor skips state transitions
            // that should happen on the raw itself.
            "tb_lite" => {
                if (tmeta.lineage_hint & LINEAGE_HINT_TB_RAW_REUSE_PARENT_FAMILY) != 0 {
                    let tmap = tags().lock().unwrap();
                    active_alias_model()
                        .find_ref_ancestor_tag(&tmap, tag)
                        .or(Some(tag))
                } else {
                    Some(tag)
                }
            }
            "sb_lite" => {
                let tmap = tags().lock().unwrap();
                active_alias_model()
                    .find_ref_ancestor_tag(&tmap, tag)
                    .or(Some(tag))
            }
            _ => {
                let tmap = tags().lock().unwrap();
                active_alias_model().find_ref_ancestor_tag(&tmap, tag)
            }
        }
    } else {
        Some(tag)
    };
    rz_profile_add_opt_field!(profile, read_tag_lookup_ns, tag_lookup_start);

    if let Some(sb_tag) = sb_tag_opt {
        let alias_check_start = rz_profile_start!(profile);
        let alias_violation = active_alias_model().check_access(
            sb_tag,
            tag,
            &tmeta,
            addr,
            size,
            AliasAccessKind::Read,
        );
        rz_profile_add_opt_field!(profile, read_alias_check_ns, alias_check_start);
        if let Some(msg) = alias_violation {
            rz_violation(
                active_alias_model().violation_kind(),
                append_location_if_enabled(msg, "RZ_LOG_LOC"),
            );
            return;
        }
    }

    // Fast path: tag-cached origin bounds + exact-base alloc lookup.
    // Slow path falls back to range lookup only when cache is missing/invalid.
    let alloc_lookup_start = rz_profile_start!(profile);
    let trace_enabled = rz_log_enabled(LogLevel::Trace);
    let cached_origin_oob = tag_origin_oob_cached(&tmeta, addr, size);
    let origin_base_alloc = alloc_from_origin_base(&tmeta);
    let mut alloc_opt: Option<(usize, AllocMeta)> = None;

    if !cached_origin_oob {
        if tag_origin_contains_access(&tmeta, addr, size) {
            alloc_opt = origin_base_alloc;
        }

        if alloc_opt.is_none() {
            alloc_opt = if trace_enabled {
                let amap = allocs().lock().unwrap();
                find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
            } else {
                live_alloc_cache::lookup_containing(addr).or_else(|| {
                    let amap = allocs().lock().unwrap();
                    find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
                })
            };
        }
    } else if origin_base_alloc.is_none() {
        // Cached origin exists but exact-base entry disappeared; revalidate via slow path.
        alloc_opt = live_alloc_cache::lookup_containing(addr).or_else(|| {
            let amap = allocs().lock().unwrap();
            find_alloc_containing(&amap, addr).map(|(base, meta)| (base, *meta))
        });
    }

    if let Some((base, ameta)) = alloc_opt {
        refresh_tag_origin_cache(tag, &mut tmeta, base, &ameta);
    }
    rz_profile_add_opt_field!(profile, read_alloc_lookup_ns, alloc_lookup_start);

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

        // If we can prove (via tag-cached origin + epoch snapshot) that this pointer was derived
        // from a particular allocation, classify this as OUT_OF_BOUNDS rather than WILD_POINTER.
        let cached_origin_alloc = if tmeta.origin_known {
            origin_base_alloc
        } else {
            None
        };
        let fallback_origin_alloc = if cached_origin_alloc.is_none() {
            let amap = allocs().lock().unwrap();
            let tmap = tags().lock().unwrap();
            origin_alloc_for_tag(&tmap, &amap, tag).map(|(base, meta)| (base, *meta))
        } else {
            None
        };
        if let Some((obase, ometa)) = cached_origin_alloc.or(fallback_origin_alloc) {
            if ometa.size != 0 && size != 0 {
                let access_end = addr.saturating_add(size);
                let alloc_end = obase.saturating_add(ometa.size);

                if addr >= obase && access_end > alloc_end {
                    if rz_allow_stack_ref_oob_noise(&tmeta, &ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_ref_root_boundary_oob_noise(&tmeta, &ometa, obase, addr, size)
                    {
                        return;
                    }
                    if rz_allow_stack_raw_root_oob_noise(&tmeta, &ometa, obase, addr, size) {
                        return;
                    }
                    if rz_allow_stack_raw_nonroot_boundary_oob_noise(
                        &tmeta, &ometa, obase, addr, size,
                    ) {
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
                    rz_violation("OUT_OF_BOUNDS", msg);
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
        rz_violation("WILD_POINTER", msg);
        return;
    };

    if !ameta.live {
        // `slot_reused` is true when we can prove the tag was minted against an older
        // allocation instance (gap >= 2). When the tag's alloc_epoch is unknown (0) we
        // conservatively treat stack accesses as ambiguous and also suppress, matching
        // the previous unconditional stack-skip behavior for that specific case.
        let slot_reused = tmeta.alloc_epoch == 0 || ameta.epoch > tmeta.alloc_epoch + 1;
        if ameta.is_stack && slot_reused && tmeta.parent != 0 && tmeta.pointee_addr == base {
            return;
        }
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && (ameta.is_stack || rz_stack_addr_hint(addr))
            && (tmeta.alloc_epoch == 0 || !tmeta.origin_known)
            && (!ameta.is_stack || slot_reused)
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
        rz_violation("USE_AFTER_DEAD", msg);
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
        rz_violation("STALE_POINTER_EPOCH_MISMATCH", msg);
        return;
    }

    // Bounds check against wide-pointer metadata (slice/str) if available.
    if bounds_len_has_explicit_byte_bounds(tmeta.bounds_len) && size != 0 {
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
                rz_violation("OUT_OF_BOUNDS", msg);
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
            rz_violation("OUT_OF_BOUNDS", msg);
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
                rz_violation("OUT_OF_BOUNDS", msg);
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
            rz_violation("OUT_OF_BOUNDS", msg);
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
pub fn __rz_ptr_read_allow_untagged(
    tag: u64,
    addr: usize,
    size: usize,
    align_req: usize,
    access_alias_exempt: u8,
) {
    if tag == 0 {
        return;
    }
    let _sb = SbSuppressGuard::enter();
    let _relax = RelaxEpochGuard::enter();
    __rz_ptr_read(tag, addr, size, align_req, access_alias_exempt);
}

fn rz_validate_ref_boundary_use(tag: u64, boundary: &str) {
    rz_validate_ref_boundary_use_at(tag, boundary, 0, BOUNDS_LEN_UNKNOWN);
}

/// Check that a returned `&mut` is still uniquely usable before the caller receives it.
fn rz_validate_mut_ref_boundary_retag(tag: u64, boundary: &str) {
    if tag == 0 {
        return;
    }

    let Some(tmeta) = tag_store::get(tag) else {
        return;
    };
    if !matches!(tmeta.kind, PtrKind::RefMut) {
        rz_validate_ref_boundary_use(tag, boundary);
        return;
    }
    if rz_ref_boundary_is_empty_precise_view(&tmeta) {
        return;
    }

    let access_size = bounds_len_bytes_or_zero(tmeta.bounds_len).min(1).max(1);
    let Some(msg) = active_alias_model().validate_ref_mut_boundary_retag(
        tag,
        &tmeta,
        tmeta.pointee_addr,
        access_size,
    ) else {
        return;
    };

    rz_violation(
        active_alias_model().violation_kind(),
        append_location_if_enabled(
            format!(
                "{boundary} invalid ref tag={tag} pointee=0x{:x} kind={:?}\n{msg}",
                tmeta.pointee_addr, tmeta.kind
            ),
            "RZ_LOG_LOC",
        ),
    );
}

fn rz_validate_ref_boundary_use_at(
    tag: u64,
    boundary: &str,
    access_addr: usize,
    access_bounds_len: usize,
) {
    if tag == 0 {
        return;
    }

    let Some(tmeta) = tag_store::get(tag) else {
        return;
    };
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return;
    }

    let access_bounds_len = if bounds_len_is_unknown(access_bounds_len) {
        tmeta.bounds_len
    } else {
        access_bounds_len
    };
    if bounds_len_is_precise_empty(access_bounds_len) {
        // Empty slice/str-style views do not perform a boundary read. Keep the tag/family, but
        // defer any actual provenance/alignment decision until a later concrete non-empty access.
        return;
    }

    let access_addr = if access_addr != 0 {
        access_addr
    } else {
        tmeta.pointee_addr
    };
    let access_size = bounds_len_bytes_or_zero(access_bounds_len).min(1).max(1);
    let Some(msg) = active_alias_model().check_access(
        tag,
        tag,
        &tmeta,
        access_addr,
        access_size,
        alias_model::AliasAccessKind::Read,
    ) else {
        return;
    };

    rz_violation(
        active_alias_model().violation_kind(),
        append_location_if_enabled(
            format!(
                "{boundary} invalid ref tag={tag} pointee=0x{:x} kind={:?}\n{msg}",
                access_addr, tmeta.kind
            ),
            "RZ_LOG_LOC",
        ),
    );
}

fn loaded_ref_validation_tag(
    exact_tag: u64,
    export_parent: u64,
    export_parent_recovered: u8,
) -> u64 {
    if export_parent_recovered != 0
        && export_parent != 0
        && tag_store::get(export_parent)
            .as_ref()
            .is_some_and(|meta| matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut))
    {
        // Recovered shadows carry a stable family in `export_parent`; the exact helper-local
        // child may be retired before the loaded reference is used again.
        export_parent
    } else {
        exact_tag
    }
}

fn rz_ref_boundary_tag_is_valid(tag: u64) -> bool {
    if tag == 0 {
        return true;
    }

    let Some(tmeta) = tag_store::get(tag) else {
        return false;
    };
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return true;
    }

    if rz_ref_boundary_is_empty_precise_view(&tmeta) {
        return true;
    }

    let access_size = bounds_len_bytes_or_zero(tmeta.bounds_len).min(1).max(1);
    active_alias_model()
        .check_access(
            tag,
            tag,
            &tmeta,
            tmeta.pointee_addr,
            access_size,
            alias_model::AliasAccessKind::Read,
        )
        .is_none()
}

fn rz_call_arg_boundary_tag_is_valid_for_addr(tag: u64, addr: usize) -> bool {
    if tag == 0 {
        return true;
    }

    let Some(tmeta) = tag_store::get(tag) else {
        return false;
    };
    if !matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return true;
    }

    if rz_ref_boundary_is_empty_precise_view(&tmeta) {
        return true;
    }

    let access_addr = if addr != 0 { addr } else { tmeta.pointee_addr };
    let access_size = bounds_len_bytes_or_zero(tmeta.bounds_len).min(1).max(1);
    active_alias_model()
        .validate_call_arg_boundary_parent(tag, &tmeta, access_addr, access_size)
        .is_none()
}

fn rz_ref_boundary_is_empty_precise_view(tmeta: &TagMeta) -> bool {
    bounds_len_is_precise_empty(tmeta.bounds_len)
}

#[inline]
fn rz_trace_call_tags_enabled() -> bool {
    std::env::var("RZ_TRACE_CALL_TAGS")
        .ok()
        .is_some_and(|v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
fn rz_trace_tag_create_enabled() -> bool {
    std::env::var("RZ_TRACE_TAG_CREATE")
        .ok()
        .is_some_and(|v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[inline]
pub(crate) fn rz_can_recover_parent_tag(tag: u64) -> bool {
    tag != 0 && active_alias_model().can_recover_parent_tag(tag)
}

fn recover_call_arg_parent_tag(addr: usize) -> u64 {
    if addr == 0 {
        return 0;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let tmap = tags().lock().unwrap();
    let mut latest_any = 0u64;
    let mut latest_mut = 0u64;
    let mut latest_any_epochless = 0u64;
    let mut latest_mut_epochless = 0u64;
    let mut seen: Vec<(u64, u64, u64, PtrKind)> = Vec::new();
    for (tag, meta) in tmap.iter() {
        if meta.parent == 0 || meta.pointee_addr != addr {
            continue;
        }
        if rz_trace_call_tags_enabled() {
            seen.push((*tag, meta.parent, meta.alloc_epoch, meta.kind));
        }
        if !rz_can_recover_parent_tag(*tag) {
            continue;
        }
        if alloc_epoch != 0 {
            if meta.alloc_epoch != alloc_epoch {
                if stack_or_tls_addr && meta.alloc_epoch == 0 {
                    if *tag > latest_any_epochless {
                        latest_any_epochless = *tag;
                    }
                    if matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut)
                        && *tag > latest_mut_epochless
                    {
                        latest_mut_epochless = *tag;
                    }
                }
                continue;
            }
        } else if !(allow_epochless_exact && meta.alloc_epoch == 0) {
            continue;
        }
        if *tag > latest_any {
            latest_any = *tag;
        }
        if matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut) && *tag > latest_mut {
            latest_mut = *tag;
        }
    }
    let recovered = if latest_mut != 0 {
        latest_mut
    } else if latest_any != 0 {
        latest_any
    } else if latest_mut_epochless != 0 {
        latest_mut_epochless
    } else if latest_any_epochless != 0 {
        latest_any_epochless
    } else {
        0
    };
    if rz_trace_call_tags_enabled() && recovered == 0 && !seen.is_empty() {
        eprintln!(
            "[rusteze-runtime][call-tag] exact-boundary lookup miss addr=0x{:x} alloc_epoch={} allow_epochless={} seen={:?}",
            addr, alloc_epoch, allow_epochless_exact, seen
        );
    }
    recovered
}

fn tag_lineage_contains_locked(
    tmap: &HashMap<u64, TagMeta>,
    descendant_tag: u64,
    ancestor_tag: u64,
) -> bool {
    if descendant_tag == 0 || ancestor_tag == 0 {
        return false;
    }

    let mut cursor = descendant_tag;
    for _ in 0..tmap.len().saturating_add(1) {
        if cursor == ancestor_tag {
            return true;
        }
        let Some(meta) = tmap.get(&cursor) else {
            break;
        };
        if meta.parent == 0 {
            break;
        }
        cursor = meta.parent;
    }
    false
}

fn recover_projected_ref_boundary_tag(addr: usize, boundary_parent_tag: u64) -> u64 {
    if addr == 0 || boundary_parent_tag == 0 {
        return 0;
    }

    let Some(boundary_parent_meta) = tag_store::get(boundary_parent_tag) else {
        return 0;
    };
    if !matches!(
        boundary_parent_meta.kind,
        PtrKind::RefShared | PtrKind::RefMut
    ) || !rz_call_arg_boundary_tag_is_valid_for_addr(boundary_parent_tag, 0)
    {
        return 0;
    }
    if boundary_parent_meta.pointee_addr == addr {
        return boundary_parent_tag;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let mut candidates: Vec<u64> = Vec::new();
    {
        let tmap = tags().lock().unwrap();
        for (tag, meta) in tmap.iter() {
            if meta.pointee_addr != addr
                || !matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut)
                || !tag_lineage_contains_locked(&tmap, *tag, boundary_parent_tag)
            {
                continue;
            }
            if alloc_epoch != 0 {
                if meta.alloc_epoch != alloc_epoch && !(stack_or_tls_addr && meta.alloc_epoch == 0)
                {
                    continue;
                }
            } else if !(allow_epochless_exact && meta.alloc_epoch == 0) {
                continue;
            }
            candidates.push(*tag);
        }
    }

    candidates.sort_unstable_by(|a, b| b.cmp(a));
    for tag in candidates {
        if rz_can_recover_parent_tag(tag) && rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr) {
            return tag;
        }
    }

    0
}

fn recover_live_boundary_tag(addr: usize) -> u64 {
    if addr == 0 {
        return 0;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let mut mut_candidates: Vec<u64> = Vec::new();
    let mut any_candidates: Vec<u64> = Vec::new();
    {
        let tmap = tags().lock().unwrap();
        for (tag, meta) in tmap.iter() {
            if meta.pointee_addr != addr {
                continue;
            }
            if alloc_epoch != 0 {
                if meta.alloc_epoch != alloc_epoch {
                    if !(stack_or_tls_addr && meta.alloc_epoch == 0) {
                        continue;
                    }
                }
            } else if !(allow_epochless_exact && meta.alloc_epoch == 0) {
                continue;
            }
            any_candidates.push(*tag);
            if matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut) {
                mut_candidates.push(*tag);
            }
        }
    }

    mut_candidates.sort_unstable_by(|a, b| b.cmp(a));
    any_candidates.sort_unstable_by(|a, b| b.cmp(a));

    for tag in mut_candidates.into_iter().chain(any_candidates.into_iter()) {
        if rz_ref_boundary_tag_is_valid(tag) {
            return tag;
        }
    }
    0
}

fn recover_oldest_live_boundary_tag(addr: usize) -> u64 {
    if addr == 0 {
        return 0;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let mut mut_candidates: Vec<u64> = Vec::new();
    let mut any_candidates: Vec<u64> = Vec::new();
    {
        let tmap = tags().lock().unwrap();
        for (tag, meta) in tmap.iter() {
            if meta.pointee_addr != addr {
                continue;
            }
            if alloc_epoch != 0 {
                if meta.alloc_epoch != alloc_epoch {
                    if !(stack_or_tls_addr && meta.alloc_epoch == 0) {
                        continue;
                    }
                }
            } else if !(allow_epochless_exact && meta.alloc_epoch == 0) {
                continue;
            }
            any_candidates.push(*tag);
            if matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut) {
                mut_candidates.push(*tag);
            }
        }
    }

    mut_candidates.sort_unstable();
    any_candidates.sort_unstable();

    for tag in mut_candidates.into_iter().chain(any_candidates.into_iter()) {
        if rz_ref_boundary_tag_is_valid(tag) {
            return tag;
        }
    }
    0
}

#[inline]
fn tag_epoch_matches_current_addr(
    meta: &TagMeta,
    alloc_epoch: u64,
    stack_or_tls_addr: bool,
    allow_epochless_exact: bool,
) -> bool {
    if bounds_len_is_precise_empty(meta.bounds_len) {
        return true;
    }
    if alloc_epoch != 0 {
        meta.alloc_epoch == alloc_epoch || (stack_or_tls_addr && meta.alloc_epoch == 0)
    } else {
        allow_epochless_exact && meta.alloc_epoch == 0
    }
}

#[inline]
fn ref_bounds_cover_ptr_value(meta: &TagMeta, ptr_addr: usize) -> bool {
    if !bounds_len_has_explicit_byte_bounds(meta.bounds_len) {
        return true;
    }
    let Some(end) = meta.pointee_addr.checked_add(meta.bounds_len) else {
        return false;
    };
    ptr_addr >= meta.pointee_addr && ptr_addr < end
}

fn ref_tag_match_rank_for_ptr_value(
    meta: &TagMeta,
    ptr_addr: usize,
    expected_kind: Option<PtrKind>,
) -> Option<u8> {
    if !matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return None;
    }
    if expected_kind.is_some_and(|kind| meta.kind != kind) {
        return None;
    }
    if meta.pointee_addr == ptr_addr {
        return Some(2);
    }
    if meta.pointee_addr == 0 || ptr_addr == 0 || !ref_bounds_cover_ptr_value(meta, ptr_addr) {
        return None;
    }

    let Some((tag_base, tag_alloc)) = lookup_alloc_snapshot(meta.pointee_addr) else {
        return None;
    };
    let Some((ptr_base, ptr_alloc)) = lookup_alloc_snapshot(ptr_addr) else {
        return None;
    };

    // A ref view can move within the same live allocation, but stale epochs must not match.
    (tag_alloc.live
        && ptr_alloc.live
        && tag_base == ptr_base
        && (tag_alloc.epoch == ptr_alloc.epoch || tag_alloc.epoch == 0 || ptr_alloc.epoch == 0)
        && (meta.alloc_epoch == ptr_alloc.epoch || meta.alloc_epoch == 0 || ptr_alloc.epoch == 0))
        .then_some(1)
}

fn exact_ref_call_arg_tag_is_valid_for_addr(tag: u64, meta: &TagMeta, addr: usize) -> bool {
    if !matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return false;
    }
    if addr != 0 && ref_tag_match_rank_for_ptr_value(meta, addr, None).is_none() {
        return false;
    }
    rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr)
}

fn projected_exact_call_arg_tag_fast_path(
    addr: usize,
    exact_tag: u64,
    boundary_parent_tag: u64,
) -> Option<u64> {
    let meta = tag_store::get(exact_tag)?;
    if !exact_ref_call_arg_tag_is_valid_for_addr(exact_tag, &meta, addr) {
        return None;
    }
    if boundary_parent_tag == 0 || boundary_parent_tag == exact_tag {
        return Some(exact_tag);
    }

    let tmap = tags().lock().unwrap();
    tag_lineage_contains_locked(&tmap, exact_tag, boundary_parent_tag).then_some(exact_tag)
}

fn recover_live_ref_boundary_tag_for_ptr_value(addr: usize) -> u64 {
    recover_live_ref_boundary_tag_for_ptr_value_with_kind(addr, None, rz_ref_boundary_tag_is_valid)
}

fn recover_live_call_arg_boundary_tag_for_ptr_value_with_kind(
    addr: usize,
    expected_kind: Option<PtrKind>,
) -> u64 {
    recover_live_ref_boundary_tag_for_ptr_value_with_kind(addr, expected_kind, |tag| {
        rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr)
    })
}

fn recover_live_ref_boundary_tag_for_ptr_value_with_kind<F>(
    addr: usize,
    expected_kind: Option<PtrKind>,
    tag_is_valid: F,
) -> u64
where
    F: Fn(u64) -> bool,
{
    if addr == 0 {
        return 0;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let mut exact_candidates: Vec<u64> = Vec::new();
    let mut view_candidates: Vec<u64> = Vec::new();
    {
        let tmap = tags().lock().unwrap();
        for (tag, meta) in tmap.iter() {
            let Some(rank) = ref_tag_match_rank_for_ptr_value(meta, addr, expected_kind) else {
                continue;
            };
            if rank == 2
                && !tag_epoch_matches_current_addr(
                    meta,
                    alloc_epoch,
                    stack_or_tls_addr,
                    allow_epochless_exact,
                )
            {
                continue;
            }
            if rank == 2 {
                exact_candidates.push(*tag);
            } else {
                view_candidates.push(*tag);
            }
        }
    }

    exact_candidates.sort_unstable_by(|a, b| b.cmp(a));
    view_candidates.sort_unstable_by(|a, b| b.cmp(a));

    for tag in exact_candidates
        .into_iter()
        .chain(view_candidates.into_iter())
    {
        if tag_is_valid(tag) {
            return tag;
        }
    }
    0
}

fn recover_newest_exact_slot_tag(addr: usize) -> u64 {
    if addr == 0 {
        return 0;
    }

    let alloc_epoch = lookup_alloc_snapshot(addr)
        .map(|(_base, meta)| meta.epoch)
        .unwrap_or(0);
    let stack_or_tls_addr = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
    let allow_epochless_exact = alloc_epoch == 0 && stack_or_tls_addr;

    let tmap = tags().lock().unwrap();
    let mut latest_any = 0u64;
    let mut latest_mut = 0u64;
    let mut latest_any_epochless = 0u64;
    let mut latest_mut_epochless = 0u64;
    for (tag, meta) in tmap.iter() {
        if meta.pointee_addr != addr {
            continue;
        }
        let is_mut_like = matches!(meta.kind, PtrKind::RefMut | PtrKind::RawMut);
        if alloc_epoch != 0 {
            if meta.alloc_epoch != alloc_epoch {
                if stack_or_tls_addr && meta.alloc_epoch == 0 {
                    if *tag > latest_any_epochless {
                        latest_any_epochless = *tag;
                    }
                    if is_mut_like && *tag > latest_mut_epochless {
                        latest_mut_epochless = *tag;
                    }
                }
                continue;
            }
        } else if !(allow_epochless_exact && meta.alloc_epoch == 0) {
            continue;
        }
        if *tag > latest_any {
            latest_any = *tag;
        }
        if is_mut_like && *tag > latest_mut {
            latest_mut = *tag;
        }
    }
    if latest_mut != 0 {
        latest_mut
    } else if latest_any != 0 {
        latest_any
    } else if latest_mut_epochless != 0 {
        latest_mut_epochless
    } else {
        latest_any_epochless
    }
}

fn recover_nearest_valid_lineage_boundary_tag(addr: usize, start_tag: u64) -> u64 {
    recover_nearest_valid_lineage_boundary_tag_with(addr, start_tag, rz_ref_boundary_tag_is_valid)
}

fn recover_nearest_valid_call_arg_lineage_boundary_tag(addr: usize, start_tag: u64) -> u64 {
    recover_nearest_valid_lineage_boundary_tag_with(addr, start_tag, |tag| {
        rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr)
    })
}

fn recover_nearest_valid_lineage_boundary_tag_with<F>(
    addr: usize,
    start_tag: u64,
    tag_is_valid: F,
) -> u64
where
    F: Fn(u64) -> bool,
{
    if addr == 0 || start_tag == 0 {
        return 0;
    }

    let mut lineage: Vec<u64> = Vec::new();
    {
        let tmap = tags().lock().unwrap();
        let mut cursor = start_tag;
        for _ in 0..tmap.len().saturating_add(1) {
            let Some(meta) = tmap.get(&cursor) else {
                break;
            };
            if meta.pointee_addr == addr
                && matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut)
            {
                lineage.push(cursor);
            }
            if meta.parent == 0 {
                break;
            }
            cursor = meta.parent;
        }
    }

    for tag in lineage {
        if rz_can_recover_parent_tag(tag) && tag_is_valid(tag) {
            return tag;
        }
    }
    0
}

/// Canonicalize the family exported by a callee for a mutated `&mut T` carrier pointee slot.
///
/// The raw callee-side tag may point at a transient inner child created during helper calls.
/// Caller-side writeback wants the nearest surviving family that future reborrows from the
/// carrier should inherit once helper-local children are gone, not an arbitrary older root ref.
fn canonical_mut_arg_ret_tag(addr: usize, tag: u64) -> u64 {
    if tag == 0 {
        return recover_live_boundary_tag(addr);
    }

    let raw_model_tag = active_alias_model().canonicalize_mut_arg_ret_tag(tag, addr);
    if raw_model_tag != 0 {
        if rz_can_recover_parent_tag(raw_model_tag) && rz_ref_boundary_tag_is_valid(raw_model_tag) {
            return raw_model_tag;
        }
        let valid_model_tag = recover_nearest_valid_lineage_boundary_tag(addr, raw_model_tag);
        if valid_model_tag != 0 {
            return valid_model_tag;
        }
    }
    let valid_raw_tag = recover_nearest_valid_lineage_boundary_tag(addr, tag);
    if valid_raw_tag != 0 {
        return valid_raw_tag;
    }

    let candidate = recover_newest_exact_slot_tag(addr).max(tag);
    let model_tag = active_alias_model().canonicalize_mut_arg_ret_tag(candidate, addr);
    if model_tag != 0 {
        let valid_model_tag = recover_nearest_valid_lineage_boundary_tag(addr, model_tag);
        if valid_model_tag != 0 {
            return valid_model_tag;
        }
    }

    let tmap = tags().lock().unwrap();
    let mut cursor = candidate;
    for _ in 0..tmap.len().saturating_add(1) {
        let Some(meta) = tmap.get(&cursor) else {
            break;
        };
        if matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut)
            && meta.pointee_addr == addr
            && rz_can_recover_parent_tag(cursor)
        {
            return cursor;
        }
        if meta.parent == 0 {
            break;
        }
        cursor = meta.parent;
    }
    drop(tmap);

    let oldest_live = recover_oldest_live_boundary_tag(addr);
    if oldest_live != 0 {
        return oldest_live;
    }

    let recovered = recover_live_boundary_tag(addr);
    if recovered != 0 {
        recovered
    } else {
        candidate
    }
}

/// Canonicalize a call-boundary argument tag to a surviving family tag for the same address.
///
/// Optimized MIR often leaves the caller local holding a transient exact child that TB has
/// already invalidated by the time we export the argument. Callee retagging wants the nearest
/// still-live boundary for that pointee slot, not the dead child.
fn canonical_call_arg_tag(addr: usize, tag: u64) -> u64 {
    if tag == 0 {
        return 0;
    }
    if addr != 0 {
        if let Some(meta) = tag_store::get(tag) {
            if exact_ref_call_arg_tag_is_valid_for_addr(tag, &meta, addr) {
                return tag;
            }
            if meta.pointee_addr != 0 && meta.pointee_addr != addr {
                let recovered = recover_call_arg_parent_tag(addr);
                if recovered != 0 {
                    return recovered;
                }
                if rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr) {
                    if matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut)
                        && rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr)
                    {
                        // By-value wrapper arguments (for example `Newtype(&mut T)` or
                        // `Option<&T>`) key the side channel by the carrier stack slot, not by the
                        // inner pointee address. Preserve the exact inner reference tag instead of
                        // dropping it to zero; the callee anchor/import path expects that tag.
                        return tag;
                    }
                    return 0;
                }
            }
        }
    }
    if rz_call_arg_boundary_tag_is_valid_for_addr(tag, addr) {
        return tag;
    }

    let model_tag = active_alias_model().canonicalize_mut_arg_ret_tag(tag, addr);
    if model_tag != 0 && rz_call_arg_boundary_tag_is_valid_for_addr(model_tag, addr) {
        return model_tag;
    }

    let valid_lineage_tag = recover_nearest_valid_call_arg_lineage_boundary_tag(addr, tag);
    if valid_lineage_tag != 0 {
        return valid_lineage_tag;
    }

    let recovered = recover_call_arg_parent_tag(addr);
    if recovered != 0 && rz_call_arg_boundary_tag_is_valid_for_addr(recovered, addr) {
        return recovered;
    }

    if rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr) {
        return 0;
    }

    tag
}

fn valid_or_recovered_call_arg_tag(addr: usize, tag: u64, allow_mut_recovery: bool) -> u64 {
    let original_kind = tag_store::get(tag).map(|meta| meta.kind);
    let canonical_tag = canonical_call_arg_tag(addr, tag);
    let Some(expected_kind @ (PtrKind::RefShared | PtrKind::RefMut)) = original_kind else {
        return canonical_tag;
    };
    let Some(meta) = tag_store::get(canonical_tag) else {
        return tag;
    };
    if meta.kind == expected_kind && rz_call_arg_boundary_tag_is_valid_for_addr(canonical_tag, addr)
    {
        if expected_kind == PtrKind::RefMut && !allow_mut_recovery && canonical_tag != tag {
            return tag;
        }
        return canonical_tag;
    }
    if expected_kind == PtrKind::RefMut && !allow_mut_recovery {
        return tag;
    }

    // A call boundary must not export a stale ref tag. Recover a live ref of the
    // same kind for the concrete pointer value; otherwise leave validation on the
    // original tag so real dead-provenance exports still report.
    let recovered =
        recover_live_call_arg_boundary_tag_for_ptr_value_with_kind(addr, Some(expected_kind));
    if recovered != 0 {
        return recovered;
    }

    tag
}

fn validate_call_arg_boundary_tag(tag: u64, addr: usize) {
    let Some(meta) = tag_store::get(tag) else {
        return;
    };
    if !matches!(meta.kind, PtrKind::RefShared | PtrKind::RefMut) {
        return;
    }
    if rz_ref_boundary_is_empty_precise_view(&meta) {
        return;
    }

    let access_addr = if addr != 0 { addr } else { meta.pointee_addr };
    let access_size = bounds_len_bytes_or_zero(meta.bounds_len).min(1).max(1);
    let Some(msg) = active_alias_model().validate_call_arg_boundary_parent(
        tag,
        &meta,
        access_addr,
        access_size,
    ) else {
        return;
    };

    rz_violation(
        active_alias_model().violation_kind(),
        append_location_if_enabled(
            format!(
                "CALL_ARG invalid ref tag={tag} pointee=0x{:x} kind={:?}\n{msg}",
                access_addr, meta.kind
            ),
            "RZ_LOG_LOC",
        ),
    );
}

fn call_arg_boundary_entry(
    addr: usize,
    exact_tag: u64,
    boundary_parent_tag: u64,
    boundary_origin: u8,
    flags: u8,
) -> CallArgTagEntry {
    let recovered_origin = boundary_origin != CALL_ARG_BOUNDARY_ORIGIN_EXACT;
    let boundary_parent_tag = if boundary_parent_tag != 0 {
        boundary_parent_tag
    } else {
        exact_tag
    };
    let tag = if recovered_origin {
        let projected_ref_tag =
            projected_exact_call_arg_tag_fast_path(addr, exact_tag, boundary_parent_tag)
                .unwrap_or_else(|| recover_projected_ref_boundary_tag(addr, boundary_parent_tag));
        let tag = valid_or_recovered_call_arg_tag(
            addr,
            if projected_ref_tag != 0 {
                projected_ref_tag
            } else {
                boundary_parent_tag
            },
            true,
        );
        validate_call_arg_boundary_tag(tag, addr);
        tag
    } else {
        let tag = valid_or_recovered_call_arg_tag(addr, exact_tag, false);
        validate_call_arg_boundary_tag(tag, addr);
        tag
    };
    CallArgTagEntry { tag, flags }
}

fn call_arg_shadow_boundary_entry(
    addr: usize,
    slot_addr: usize,
    flags: u8,
) -> (CallArgTagEntry, u64, u64, u8) {
    let exact_tag = ptr_shadow::load_tag_for_ptr_value(slot_addr, addr);
    let export_parent = ptr_shadow::load_export_parent_for_ptr_value(slot_addr, addr);
    let recovered = ptr_shadow::load_export_parent_recovered_for_ptr_value(slot_addr, addr);
    let boundary_parent = if export_parent != 0 {
        export_parent
    } else {
        exact_tag
    };
    (
        call_arg_boundary_entry(addr, exact_tag, boundary_parent, recovered, flags),
        exact_tag,
        boundary_parent,
        recovered,
    )
}

/// Push explicit call-boundary pointer state so callees can retag on entry.
///
/// `exact_tag` is the caller local's current tag. `boundary_parent_tag` is the family the callee
/// should inherit when the local came from an earlier boundary import/recovery. Exact-origin
/// locals validate the exact tag first; recovered-origin locals validate/export the boundary
/// parent instead of a transient local child.
#[no_mangle]
pub extern "C" fn __rz_push_call_arg_boundary_tag(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
    exact_tag: u64,
    boundary_parent_tag: u64,
    boundary_origin: u8,
    flags: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_push);
    let _g = RzRuntimeGuard::enter();
    let boundary_parent_tag = if boundary_parent_tag != 0 {
        boundary_parent_tag
    } else {
        exact_tag
    };
    if exact_tag == 0 && boundary_parent_tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        if rz_trace_call_tags_enabled() {
            eprintln!(
                "[rusteze-runtime][call-tag] push callee={} arg={} addr=0x{:x} exact=0 boundary_parent=0 origin={} tag=0 flags=0x{:x} skipped=zero",
                callee_id, arg_index, addr, boundary_origin, flags
            );
        }
        return;
    }
    let entry =
        call_arg_boundary_entry(addr, exact_tag, boundary_parent_tag, boundary_origin, flags);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] push callee={} arg={} addr=0x{:x} exact={} boundary_parent={} origin={} tag={} flags=0x{:x}",
            callee_id, arg_index, addr, exact_tag, boundary_parent_tag, boundary_origin, entry.tag, flags
        );
    }
    if entry.tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let thread_id = std::thread::current().id();
    call_arg_tags()
        .lock()
        .unwrap()
        .insert((thread_id, callee_id, arg_index, addr), entry);
}

/// Push call-boundary state for a projected pointer slot.
///
/// The instrumentation gives both the pointer value (`addr`) and the slot holding it, so runtime
/// transport can load the exact shadow entry for that value instead of guessing from the base local.
#[no_mangle]
pub extern "C" fn __rz_push_call_arg_boundary_shadow_tag(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
    slot_addr: usize,
    flags: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_push);
    let _g = RzRuntimeGuard::enter();
    let (entry, exact_tag, boundary_parent, recovered) =
        call_arg_shadow_boundary_entry(addr, slot_addr, flags);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] push-shadow callee={} arg={} addr=0x{:x} slot=0x{:x} exact={} boundary_parent={} origin={} tag={} flags=0x{:x}",
            callee_id,
            arg_index,
            addr,
            slot_addr,
            exact_tag,
            boundary_parent,
            recovered,
            entry.tag,
            flags
        );
    }
    if entry.tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let thread_id = std::thread::current().id();
    call_arg_tags()
        .lock()
        .unwrap()
        .insert((thread_id, callee_id, arg_index, addr), entry);
}

/// Open a caller-side side channel for an unresolved function-pointer/vtable call.
#[no_mangle]
pub extern "C" fn __rz_begin_indirect_call_arg_scope() {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_scope);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    dynamic_call_arg_scopes()
        .lock()
        .unwrap()
        .entry(thread_id)
        .or_default()
        .push(DynamicCallArgScope::default());
}

/// Close the caller-side side channel for an unresolved function-pointer/vtable call.
#[no_mangle]
pub extern "C" fn __rz_end_indirect_call_arg_scope() {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_scope);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
    let Some(stack) = scopes.get_mut(&thread_id) else {
        return;
    };
    stack.pop();
    if stack.is_empty() {
        scopes.remove(&thread_id);
    }
}

/// Push call-boundary pointer state for an unresolved function-pointer/vtable call.
#[no_mangle]
pub extern "C" fn __rz_push_indirect_call_arg_boundary_tag(
    arg_index: u64,
    addr: usize,
    exact_tag: u64,
    boundary_parent_tag: u64,
    boundary_origin: u8,
    flags: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_push);
    let _g = RzRuntimeGuard::enter();
    let boundary_parent_tag = if boundary_parent_tag != 0 {
        boundary_parent_tag
    } else {
        exact_tag
    };
    if exact_tag == 0 && boundary_parent_tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        if rz_trace_call_tags_enabled() {
            eprintln!(
                "[rusteze-runtime][call-tag] push indirect arg={} addr=0x{:x} exact=0 boundary_parent=0 origin={} tag=0 flags=0x{:x} skipped=zero",
                arg_index, addr, boundary_origin, flags
            );
        }
        return;
    }
    let entry =
        call_arg_boundary_entry(addr, exact_tag, boundary_parent_tag, boundary_origin, flags);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] push indirect arg={} addr=0x{:x} exact={} boundary_parent={} origin={} tag={} flags=0x{:x}",
            arg_index, addr, exact_tag, boundary_parent_tag, boundary_origin, entry.tag, flags
        );
    }
    if entry.tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let thread_id = std::thread::current().id();
    let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
    let stack = scopes.entry(thread_id).or_default();
    if stack.is_empty() {
        stack.push(DynamicCallArgScope::default());
    }
    if let Some(scope) = stack.last_mut() {
        scope.arg_tags.insert((arg_index, addr), entry);
    }
}

/// Push projected-slot call-boundary state for an unresolved function-pointer/vtable call.
#[no_mangle]
pub extern "C" fn __rz_push_indirect_call_arg_boundary_shadow_tag(
    arg_index: u64,
    addr: usize,
    slot_addr: usize,
    flags: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_push);
    let _g = RzRuntimeGuard::enter();
    let (entry, exact_tag, boundary_parent, recovered) =
        call_arg_shadow_boundary_entry(addr, slot_addr, flags);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] push-shadow indirect arg={} addr=0x{:x} slot=0x{:x} exact={} boundary_parent={} origin={} tag={} flags=0x{:x}",
            arg_index,
            addr,
            slot_addr,
            exact_tag,
            boundary_parent,
            recovered,
            entry.tag,
            flags
        );
    }
    if entry.tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let thread_id = std::thread::current().id();
    let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
    let stack = scopes.entry(thread_id).or_default();
    if stack.is_empty() {
        stack.push(DynamicCallArgScope::default());
    }
    if let Some(scope) = stack.last_mut() {
        scope.arg_tags.insert((arg_index, addr), entry);
    }
}

/// Push one pointer-leaf shadow for an unresolved function-pointer/vtable call.
#[no_mangle]
pub extern "C" fn __rz_push_indirect_call_arg_leaf_shadow(
    arg_index: u64,
    leaf_key: u64,
    slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_leaf);
    let _g = RzRuntimeGuard::enter();
    let Some(shadow) = current_slot_shadow(slot_addr) else {
        return;
    };
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] push indirect leaf arg={} leaf={} slot=0x{:x} tag={}",
            arg_index, leaf_key, slot_addr, shadow.0
        );
    }
    let thread_id = std::thread::current().id();
    let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
    let stack = scopes.entry(thread_id).or_default();
    if stack.is_empty() {
        stack.push(DynamicCallArgScope::default());
    }
    if let Some(scope) = stack.last_mut() {
        scope
            .leaf_shadows
            .insert((arg_index, leaf_key), (slot_addr, shadow));
    }
}

/// Validate a non-pointer by-value call argument carrier's inner reference tag.
#[no_mangle]
pub extern "C" fn __rz_validate_call_arg_tag(tag: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_take);
    let _g = RzRuntimeGuard::enter();
    validate_call_arg_boundary_tag(tag, 0);
}

/// Push the exact shadow of one internal pointer leaf of a by-value aggregate argument.
#[no_mangle]
pub extern "C" fn __rz_push_call_arg_leaf_shadow(
    callee_id: u64,
    arg_index: u64,
    leaf_key: u64,
    slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_leaf);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    let Some(shadow) = current_slot_shadow(slot_addr) else {
        return;
    };
    call_arg_leaf_shadows().lock().unwrap().insert(
        (thread_id, callee_id, arg_index, leaf_key),
        (slot_addr, shadow),
    );
}

/// Drop unconsumed exact call-argument leaf shadows for `callee_id`.
#[no_mangle]
pub extern "C" fn __rz_clear_call_arg_leaf_shadows(callee_id: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_leaf);
    let _g = RzRuntimeGuard::enter();
    clear_unconsumed_call_arg_leaf_shadows(callee_id);
}

/// Open the explicit boundary key for a direct instrumented call.
#[no_mangle]
pub extern "C" fn __rz_begin_direct_call_boundary(boundary_id: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_scope);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-boundary] begin direct boundary={}",
            boundary_id
        );
    }
    pending_direct_call_boundaries()
        .lock()
        .unwrap()
        .entry(thread_id)
        .or_default()
        .push(boundary_id);
}

/// Clean up a direct boundary key if the callee never consumed it.
#[no_mangle]
pub extern "C" fn __rz_end_direct_call_boundary(boundary_id: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_scope);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-boundary] end direct boundary={}",
            boundary_id
        );
    }
    {
        let mut pending = pending_direct_call_boundaries().lock().unwrap();
        if let Some(stack) = pending.get_mut(&thread_id) {
            if stack.last().copied() == Some(boundary_id) {
                stack.pop();
            } else if let Some(pos) = stack.iter().rposition(|id| *id == boundary_id) {
                stack.remove(pos);
            }
            if stack.is_empty() {
                pending.remove(&thread_id);
            }
        }
    }
    if let Some(activation) = exit_direct_call_boundary(boundary_id) {
        finish_call_boundary_activation(activation);
    }
}

/// Take (consume) a pushed pointer-argument tag for a callee/arg/address triple.
#[no_mangle]
pub extern "C" fn __rz_take_call_arg_tag(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
    _require_mut: u8,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_take);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    let boundary_id = active_call_boundary_id(callee_id);
    let direct_entry = {
        let mut tags = call_arg_tags().lock().unwrap();
        let mut matched_callee_id = boundary_id;
        let allow_cross_callee_fallback = rz_stack_addr_hint(addr) || rz_tls_addr_hint(addr);
        let entry = tags
            .remove(&(thread_id, boundary_id, arg_index, addr))
            .unwrap_or_else(|| {
                if !allow_cross_callee_fallback {
                    return CallArgTagEntry::default();
                }
                let mut fallback_matches = tags
                    .keys()
                    .filter(|(tid, _cid, idx, other_addr)| {
                        *tid == thread_id && *idx == arg_index && *other_addr == addr
                    })
                    .copied();
                let first = fallback_matches.next();
                if fallback_matches.next().is_some() {
                    return CallArgTagEntry::default();
                }
                if let Some((_, fallback_callee_id, _, _)) = first {
                    matched_callee_id = fallback_callee_id;
                    return tags
                        .remove(&(thread_id, fallback_callee_id, arg_index, addr))
                        .unwrap_or_default();
                }
                CallArgTagEntry::default()
            });
        let protect_arg = (entry.flags & CALL_ARG_FLAG_NO_PROTECTOR) == 0;
        let inplace_alias_parent = if entry.tag != 0 && protect_arg {
            tags.iter()
                .find(|((tid, cid, other_arg, other_addr), other_entry)| {
                    *tid == thread_id
                        && *cid == matched_callee_id
                        && *other_arg != arg_index
                        && *other_addr == addr
                        && (other_entry.flags & CALL_ARG_FLAG_INPLACE_EXACT_SOURCE) != 0
                        && (other_entry.flags & CALL_ARG_FLAG_NO_PROTECTOR) == 0
                })
                .map(|(_key, other_entry)| other_entry.tag)
        } else {
            None
        };
        (entry, inplace_alias_parent, matched_callee_id)
    };
    let (entry, inplace_alias_parent, matched_callee_id) = if direct_entry.0.tag != 0 {
        direct_entry
    } else {
        let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
        let dynamic_entry = scopes
            .get_mut(&thread_id)
            .and_then(|stack| stack.last_mut())
            .and_then(|scope| {
                let entry = scope.arg_tags.remove(&(arg_index, addr))?;
                scope.consumed = true;
                let protect_arg = (entry.flags & CALL_ARG_FLAG_NO_PROTECTOR) == 0;
                let inplace_alias_parent = if entry.tag != 0 && protect_arg {
                    scope
                        .arg_tags
                        .iter()
                        .find(|((other_arg, other_addr), other_entry)| {
                            *other_arg != arg_index
                                && *other_addr == addr
                                && (other_entry.flags & CALL_ARG_FLAG_INPLACE_EXACT_SOURCE) != 0
                                && (other_entry.flags & CALL_ARG_FLAG_NO_PROTECTOR) == 0
                        })
                        .map(|(_key, other_entry)| other_entry.tag)
                } else {
                    None
                };
                Some((entry, inplace_alias_parent))
            });
        if let Some((entry, inplace_alias_parent)) = dynamic_entry {
            (entry, inplace_alias_parent, boundary_id)
        } else {
            direct_entry
        }
    };
    let tag = entry.tag;
    let protect_arg = (entry.flags & CALL_ARG_FLAG_NO_PROTECTOR) == 0;
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-tag] take callee={} matched_callee={} arg={} addr=0x{:x} -> {} protector={} inplace_alias_parent={}",
            callee_id,
            matched_callee_id,
            arg_index,
            addr,
            tag,
            protect_arg,
            inplace_alias_parent.unwrap_or(0)
        );
    }
    if tag != 0 {
        capture_call_arg_leaf_shadows_for_arg(thread_id, callee_id, matched_callee_id, arg_index);
    }
    if tag != 0 && protect_arg {
        active_alias_model().on_call_arg_taken(matched_callee_id, tag);
        if let Some(alias_parent_tag) = inplace_alias_parent {
            active_alias_model().on_call_arg_inplace_alias(
                matched_callee_id,
                alias_parent_tag,
                addr,
            );
        }
    }
    tag
}

/// Take (consume) a pushed inner tag for a non-pointer carrier argument.
#[no_mangle]
pub extern "C" fn __rz_take_call_arg_tag_anchor(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_take);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    let boundary_id = active_call_boundary_id(callee_id);
    let tag = {
        let mut tags = call_arg_tags().lock().unwrap();
        tags.remove(&(thread_id, boundary_id, arg_index, addr))
            .or_else(|| {
                tags.iter()
                    .find(|((tid, cid, idx, _slot_addr), _)| {
                        *tid == thread_id && *cid == boundary_id && *idx == arg_index
                    })
                    .map(|(key, _)| *key)
                    .and_then(|key| tags.remove(&key))
            })
            .unwrap_or_default()
            .tag
    };
    if tag != 0 {
        active_alias_model().on_call_arg_anchor_taken(boundary_id, tag);
    }
    tag
}

/// Restore one exact pointer-leaf shadow into an argument slot.
///
/// Missing or stale side-channel facts are ignored; real byte writes clear shadow separately.
#[no_mangle]
pub extern "C" fn __rz_take_call_arg_leaf_shadow(
    callee_id: u64,
    arg_index: u64,
    leaf_key: u64,
    slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, call_arg_leaf);
    let _g = RzRuntimeGuard::enter();
    let thread_id = std::thread::current().id();
    let boundary_id = active_call_boundary_id(callee_id);
    let shadow = {
        let mut leafs = call_arg_leaf_shadows().lock().unwrap();
        leafs
            .remove(&(thread_id, boundary_id, arg_index, leaf_key))
            .map(|(_slot_addr, shadow)| shadow)
    }
    .or_else(|| {
        let mut scopes = dynamic_call_arg_scopes().lock().unwrap();
        scopes
            .get_mut(&thread_id)
            .and_then(|stack| {
                stack
                    .iter_mut()
                    .rev()
                    .find_map(|scope| scope.leaf_shadows.remove(&(arg_index, leaf_key)))
            })
            .map(|(_slot_addr, shadow)| shadow)
    })
    .or_else(|| scoped_call_arg_leaf_shadow(thread_id, leaf_key, slot_addr));
    restore_leaf_shadow(slot_addr, shadow);
}

/// Export the post-call family for a non-pointer carrier pointee mutated through `&mut T`.
#[no_mangle]
pub extern "C" fn __rz_push_mut_arg_ret_tag(callee_id: u64, arg_index: u64, addr: usize, tag: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, mut_arg_ret_push);
    let _g = RzRuntimeGuard::enter();
    let boundary_id = active_call_boundary_id(callee_id);
    let raw_tag = tag;
    let newest_exact = recover_newest_exact_slot_tag(addr);
    let tag = if raw_tag == 0 {
        canonical_mut_arg_ret_tag(addr, raw_tag)
    } else {
        raw_tag
    };
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][mut-arg-ret] push callee={} arg={} addr=0x{:x} raw_tag={} newest_exact={} tag={}",
            boundary_id, arg_index, addr, raw_tag, newest_exact, tag
        );
    }
    if tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    if tag != 0 {
        remember_mut_arg_ret_boundary_lineage(tag);
        apply_or_defer_boundary_survivor(boundary_id, tag, addr, BoundarySurvivorKind::MutArgRet);
    }
    let thread_id = std::thread::current().id();
    mut_arg_ret_tags()
        .lock()
        .unwrap()
        .insert((thread_id, boundary_id, arg_index, addr), tag);
}

/// Consume the callee-exported family for a non-pointer carrier pointee after a call returns.
#[no_mangle]
pub extern "C" fn __rz_take_mut_arg_ret_tag(callee_id: u64, arg_index: u64, addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, mut_arg_ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let raw_tag = mut_arg_ret_tags()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id, arg_index, addr))
        .unwrap_or(0);
    let tag = canonical_mut_arg_ret_tag(addr, raw_tag);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][mut-arg-ret] take callee={} arg={} addr=0x{:x} raw_tag={} tag={}",
            callee_id, arg_index, addr, raw_tag, tag
        );
    }
    clear_boundary_survivor_tags(callee_id);
    tag
}

/// Consume the callee-exported family for a pointer-only `&mut T` writeback after a call returns.
///
/// Unlike `__rz_take_mut_arg_ret_tag`, a missing side-channel export is meaningful here:
/// the caller must keep its existing local tag. Preserve `0` instead of recovering a new local
/// family from the address.
#[no_mangle]
pub extern "C" fn __rz_take_mut_arg_ret_tag_or_zero(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, mut_arg_ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let raw_tag = mut_arg_ret_tags()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id, arg_index, addr))
        .unwrap_or(0);
    let tag = if raw_tag == 0 {
        0
    } else {
        canonical_mut_arg_ret_tag(addr, raw_tag)
    };
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][mut-arg-ret] take-or-zero callee={} arg={} addr=0x{:x} raw_tag={} tag={}",
            callee_id, arg_index, addr, raw_tag, tag
        );
    }
    clear_boundary_survivor_tags(callee_id);
    tag
}

/// Push the exact shadow of one internal pointer leaf of a `&mut T` carrier pointee so the
/// caller can recreate that slot shadow after the call returns.
#[no_mangle]
pub extern "C" fn __rz_push_mut_arg_ret_leaf_shadow(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
    leaf_key: u64,
    slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, mut_arg_ret_push);
    let _g = RzRuntimeGuard::enter();
    let boundary_id = active_call_boundary_id(callee_id);
    let Some((tag, ref_ancestor, export_parent, export_parent_recovered)) =
        current_slot_shadow(slot_addr)
    else {
        return;
    };
    if active_alias_model().name() == "sb_lite" {
        rz_validate_ref_boundary_use(tag, "RET");
    }
    if tag != 0 {
        remember_mut_arg_ret_boundary_lineage(tag);
        apply_or_defer_boundary_survivor(boundary_id, tag, addr, BoundarySurvivorKind::MutArgRet);
    }
    let thread_id = std::thread::current().id();
    mut_arg_ret_leaf_shadows().lock().unwrap().insert(
        (thread_id, boundary_id, arg_index, addr, leaf_key),
        (tag, ref_ancestor, export_parent, export_parent_recovered),
    );
}

/// Take one exported `&mut T` carrier leaf shadow and recreate the caller destination slot
/// shadow.
#[no_mangle]
pub extern "C" fn __rz_take_mut_arg_ret_leaf_shadow(
    callee_id: u64,
    arg_index: u64,
    addr: usize,
    leaf_key: u64,
    slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, mut_arg_ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let shadow = mut_arg_ret_leaf_shadows()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id, arg_index, addr, leaf_key));
    restore_leaf_shadow(slot_addr, shadow);
    clear_boundary_survivor_tags(callee_id);
}

/// Push a return-tag into a runtime side-channel so the caller can recover it after the call.
#[no_mangle]
pub extern "C" fn __rz_push_ret_tag(callee_id: u64, addr: usize, tag: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_push);
    let _g = RzRuntimeGuard::enter();
    if tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let boundary_id = active_call_boundary_id(callee_id);
    let survivor_kind = if return_tag_is_mut_arg_ret_boundary_survivor(tag) {
        BoundarySurvivorKind::MutArgRet
    } else {
        BoundarySurvivorKind::Return
    };
    validate_and_export_return_tag(boundary_id, tag, addr, survivor_kind);
    let thread_id = std::thread::current().id();
    ret_tags()
        .lock()
        .unwrap()
        .insert((thread_id, boundary_id, addr), tag);
}

/// Push the exact shadow of one returned carrier leaf so the caller can recreate its slot shadow.
#[no_mangle]
pub extern "C" fn __rz_push_ret_leaf_shadow(
    callee_id: u64,
    leaf_key: u64,
    slot_addr: usize,
    leaf_is_ref: u8,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_push);
    let _g = RzRuntimeGuard::enter();
    let boundary_id = active_call_boundary_id(callee_id);
    let shadow = canonical_return_leaf_shadow(slot_addr, leaf_is_ref != 0);
    if !shadow_is_present(shadow) {
        return;
    }
    let (ret_tag, ret_ref_ancestor, ret_export_parent, ret_export_parent_recovered) = shadow;
    let survivor_kind = if return_tag_is_mut_arg_ret_boundary_survivor(ret_tag) {
        BoundarySurvivorKind::MutArgRet
    } else {
        BoundarySurvivorKind::ReturnLeaf
    };
    validate_and_export_return_tag(boundary_id, ret_tag, 0, survivor_kind);
    let thread_id = std::thread::current().id();
    ret_leaf_shadows().lock().unwrap().insert(
        (thread_id, boundary_id, leaf_key),
        (
            ret_tag,
            ret_ref_ancestor,
            ret_export_parent,
            ret_export_parent_recovered,
        ),
    );
}

/// Validate a non-pointer return carrier's inner reference tag at the return boundary.
#[no_mangle]
pub extern "C" fn __rz_validate_ret_tag(callee_id: u64, tag: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_push);
    let _g = RzRuntimeGuard::enter();
    if tag == 0 {
        rz_profile_inc_field!(profile, call_arg_zero_skipped);
        return;
    }
    let boundary_id = active_call_boundary_id(callee_id);
    let survivor_kind = if return_tag_is_mut_arg_ret_boundary_survivor(tag) {
        BoundarySurvivorKind::MutArgRet
    } else {
        BoundarySurvivorKind::Return
    };
    validate_and_export_return_tag(boundary_id, tag, 0, survivor_kind);
    if tag != 0 {
        let thread_id = std::thread::current().id();
        ret_tags()
            .lock()
            .unwrap()
            .insert((thread_id, boundary_id, 0), tag);
    }
}

/// Validate a reference tag restored from pointer-shadow memory.
#[no_mangle]
pub extern "C" fn __rz_validate_loaded_ref_tag(
    tag: u64,
    export_parent: u64,
    export_parent_recovered: u8,
    pointee_addr: usize,
) {
    let _g = RzRuntimeGuard::enter();
    let access_bounds_len = tag_store::get(tag)
        .map(|meta| meta.bounds_len)
        .unwrap_or(BOUNDS_LEN_UNKNOWN);
    let validation_tag = loaded_ref_validation_tag(tag, export_parent, export_parent_recovered);
    rz_validate_ref_boundary_use_at(validation_tag, "LOAD", pointee_addr, access_bounds_len);
}

/// Require a nonzero tag when loading a pointer/reference value from memory.
#[no_mangle]
pub extern "C" fn __rz_require_loaded_ptr_tag(tag: u64) {
    let _g = RzRuntimeGuard::enter();
    if tag == 0 {
        rz_violation(
            "WILD_POINTER",
            append_location_if_enabled(
                "READ invalid loaded ref tag=0 size=1\nreason=NO_PROVENANCE_LOAD".to_string(),
                "RZ_LOG_LOC",
            ),
        );
    }
}

/// Take (consume) a pushed return-tag for a callee/return-address pair.
#[no_mangle]
pub extern "C" fn __rz_take_ret_tag(callee_id: u64, addr: usize) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let tag = ret_tags()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id, addr))
        .unwrap_or(0);
    clear_boundary_survivor_tags(callee_id);
    tag
}

/// Take one returned carrier-leaf shadow and recreate the caller destination slot shadow.
#[no_mangle]
pub extern "C" fn __rz_take_ret_leaf_shadow(callee_id: u64, leaf_key: u64, slot_addr: usize) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let shadow = ret_leaf_shadows()
        .lock()
        .unwrap()
        .remove(&(thread_id, callee_id, leaf_key));
    restore_leaf_shadow(slot_addr, shadow);
    clear_boundary_survivor_tags(callee_id);
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
    align_req: usize,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ret_take);
    let _g = RzRuntimeGuard::enter();
    finalize_direct_call_boundary(callee_id);
    let thread_id = std::thread::current().id();
    let tag = {
        ret_tags()
            .lock()
            .unwrap()
            .remove(&(thread_id, callee_id, addr))
            .unwrap_or(0)
    };
    clear_boundary_survivor_tags(callee_id);
    if tag != 0 {
        return tag;
    }
    // Fallback: synthesize a fresh raw-pointer tag rooted at this address.
    __record_raw_ptr_creation(addr, is_mut, 0, alias_exempt, bounds_len, align_req)
}

/// Open the per-activation call-argument payload scope for an instrumented function.
#[no_mangle]
pub extern "C" fn __rz_enter_fn(callee_id: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, fn_enter);
    let _g = RzRuntimeGuard::enter();
    // The callee keeps using its function id in MIR, while side channels use the caller's
    // boundary key for this activation.
    let boundary_id = enter_call_boundary(callee_id);
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-boundary] enter callee={} boundary={}",
            callee_id, boundary_id
        );
    }
    enter_call_arg_leaf_scope(callee_id);
}

/// Notify runtime alias models that the current instrumented function is exiting.
#[no_mangle]
pub extern "C" fn __rz_exit_fn(callee_id: u64) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, fn_exit);
    let _g = RzRuntimeGuard::enter();
    if active_call_boundary_activation(callee_id).is_some_and(|activation| activation.direct) {
        if rz_trace_call_tags_enabled() {
            eprintln!(
                "[rusteze-runtime][call-boundary] defer exit callee={}",
                callee_id
            );
        }
        return;
    }
    let activation = exit_call_boundary(callee_id);
    let boundary_id = activation.boundary_id;
    if rz_trace_call_tags_enabled() {
        eprintln!(
            "[rusteze-runtime][call-boundary] exit callee={} boundary={}",
            callee_id, boundary_id
        );
    }
    clear_unconsumed_call_arg_leaf_shadows(boundary_id);
    exit_call_arg_leaf_scope(callee_id);
    active_alias_model().on_call_exit(boundary_id);
    apply_deferred_boundary_survivor_exports(boundary_id);
    clear_boundary_survivor_tags(boundary_id);
}

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize, u8, u64, u8, usize, usize) -> u64 = $sym;
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
    align_req: usize,
) -> u64 {
    __record_ref_creation_with_extent(
        pointee_addr,
        is_mut,
        parent_tag,
        alias_exempt,
        bounds_len,
        align_req,
        0,
        0,
    )
}

#[no_mangle]
pub extern "C" fn __record_ref_creation_with_extent(
    pointee_addr: usize,
    is_mut: u8,
    parent_tag: u64,
    alias_exempt: u8,
    bounds_len: usize,
    align_req: usize,
    interior_mut_extent_base: usize,
    interior_mut_extent_len: usize,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ref_create);
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let kind = if is_mut != 0 {
        PtrKind::RefMut
    } else {
        PtrKind::RefShared
    };
    let normalized_const_end_ref = pointee_addr != 0
        && pointee_addr != normalize_const_end_ref_pointee(pointee_addr, parent_tag, bounds_len);
    let pointee_addr = normalize_const_end_ref_pointee(pointee_addr, parent_tag, bounds_len);
    // `alias_exempt` is a creation flag bitfield emitted by instrumentation.
    let alias_exempt_flag = (alias_exempt & CREATION_FLAG_ALIAS_EXEMPT) != 0;
    let dynamic_bounds = (alias_exempt & CREATION_FLAG_REF_DYNAMIC_BOUNDS) != 0;
    let requested_align = align_req;
    let promised_align = if bounds_len_is_zero_sized_known(bounds_len) {
        0
    } else {
        rz_promised_alignment_for_addr(
            pointee_addr,
            lookup_alloc_snapshot(pointee_addr)
                .map(|(_, meta)| meta.epoch)
                .unwrap_or(0),
        )
    };
    let align_req =
        rz_effective_ref_align_req(requested_align, parent_tag, pointee_addr).max(promised_align);
    let required_align = if requested_align != 0 {
        requested_align
    } else {
        align_req
    };

    let validate_start = rz_profile_start!(profile);
    if required_align != 0 && align_req != 0 && align_req < required_align {
        rz_check_alignment(
            "REF_CREATE",
            tag,
            pointee_addr,
            bounds_len_bytes_or_zero(bounds_len).max(1),
            align_req,
            required_align,
            None,
        );
    }
    if !alias_exempt_flag {
        if let Some((vk, msg)) = rz_validate_ref_creation_addr(
            pointee_addr,
            kind,
            parent_tag,
            bounds_len,
            dynamic_bounds,
        ) {
            rz_violation(vk, append_location_if_enabled(msg, "RZ_LOG_LOC"));
        }
    }
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
    rz_profile_add_elapsed_field!(profile, ref_create_validate_ns, validate_start);

    // IMPORTANT: On retagging/reborrows (parent_tag != 0), prefer inheriting the parent's
    // allocation snapshot to keep stack-slot reuse detectable.
    // Exception: if the new ref clearly points into a different allocation than the parent
    // pointee, refresh to the pointee allocation snapshot (common in projection-heavy code).
    let mut alloc_is_stack = false;
    let mut alloc_size = 0usize;
    let alloc_snapshot_start = rz_profile_start!(profile);
    let (
        mut alloc_epoch,
        mut alloc_live_at_creation,
        mut inherited_bounds_len,
        mut resolved_parent_tag,
    ) = if parent_tag != 0 {
        let (parent_epoch, parent_live, parent_pointee, inherited_bounds_len) =
            tag_store::get(parent_tag)
                .as_ref()
                .map(|p| {
                    (
                        p.alloc_epoch,
                        p.alloc_live_at_creation,
                        Some(p.pointee_addr),
                        p.bounds_len,
                    )
                })
                .unwrap_or((0, false, None, BOUNDS_LEN_UNKNOWN));

        if let Some(parent_pointee) = parent_pointee {
            let parent_alloc = lookup_alloc_snapshot(parent_pointee);
            let pointee_alloc = lookup_alloc_snapshot(pointee_addr);
            match (parent_alloc, pointee_alloc) {
                (Some((parent_base, _parent_meta)), Some((pointee_base, pointee_meta))) => {
                    alloc_is_stack = pointee_meta.is_stack;
                    alloc_size = pointee_meta.size;
                    if parent_base != pointee_base
                        || (parent_epoch != 0
                            && pointee_meta.epoch != 0
                            && parent_epoch != pointee_meta.epoch)
                    {
                        (
                            pointee_meta.epoch,
                            pointee_meta.live,
                            BOUNDS_LEN_UNKNOWN,
                            parent_tag,
                        )
                    } else if parent_epoch == 0 && pointee_meta.epoch != 0 {
                        // Parent lineage is correct, but its allocation snapshot was lost.
                        // Keep the parent tag while refreshing the epoch/live snapshot from the
                        // actual pointee allocation so later exact-address lookups still work.
                        (
                            pointee_meta.epoch,
                            pointee_meta.live,
                            inherited_bounds_len,
                            parent_tag,
                        )
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
                        (0, false, BOUNDS_LEN_UNKNOWN, 0)
                    } else {
                        (parent_epoch, parent_live, inherited_bounds_len, parent_tag)
                    }
                }
                (None, Some((_pointee_base, pointee_meta))) => {
                    alloc_is_stack = pointee_meta.is_stack;
                    alloc_size = pointee_meta.size;
                    (
                        pointee_meta.epoch,
                        pointee_meta.live,
                        BOUNDS_LEN_UNKNOWN,
                        parent_tag,
                    )
                }
                (None, None) => {
                    // Parent metadata already detached from a concrete allocation.
                    // Continuing to inherit it creates cascading false OOB/UAF reports.
                    if parent_epoch != 0 {
                        (0, false, BOUNDS_LEN_UNKNOWN, 0)
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
        lookup_alloc_snapshot(pointee_addr)
            .map(|(_base, m)| {
                alloc_is_stack = m.is_stack;
                alloc_size = m.size;
                if m.is_stack && !m.live {
                    (0, false, BOUNDS_LEN_UNKNOWN, 0)
                } else {
                    (m.epoch, m.live, BOUNDS_LEN_UNKNOWN, 0)
                }
            })
            .unwrap_or((0, false, BOUNDS_LEN_UNKNOWN, 0))
    };
    rz_profile_add_elapsed_field!(profile, ref_create_alloc_snapshot_ns, alloc_snapshot_start);

    let bounds_len = if bounds_len_is_known(bounds_len) {
        bounds_len
    } else {
        inherited_bounds_len
    };
    let mut interior_mut_extent_base = interior_mut_extent_base;
    let mut interior_mut_extent_len = interior_mut_extent_len;
    if interior_mut_extent_len == 0 && resolved_parent_tag != 0 {
        if let Some(parent_meta) = tag_store::get(resolved_parent_tag) {
            interior_mut_extent_base = parent_meta.interior_mut_extent_base;
            interior_mut_extent_len = parent_meta.interior_mut_extent_len;
        }
    }
    let insert_start = rz_profile_start!(profile);
    let (origin_known, origin_base, origin_end) =
        snapshot_tag_origin(pointee_addr, resolved_parent_tag);

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: resolved_parent_tag,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt_flag,
        lineage_hint: (alias_exempt & CREATION_REF_LINEAGE_HINT_MASK)
            | if normalized_const_end_ref {
                LINEAGE_HINT_CONST_END_REF_NORMALIZED
            } else {
                0
            },
        exposed_provenance_root: false,
        bounds_len,
        interior_mut_extent_base,
        interior_mut_extent_len,
        align_req,
        origin_known,
        origin_base,
        origin_end,
    };
    let tag_store_insert_start = rz_profile_start!(profile);
    tag_store::insert(tag, tmeta);
    if tmeta.alloc_epoch != 0 && tmeta.origin_known {
        tag_store::remember_alloc_epoch_tag(tmeta.origin_base, tmeta.alloc_epoch, tag);
    }
    tag_pruning::remember_live_tag(tag, &tmeta);
    rz_profile_add_elapsed_field!(
        profile,
        ref_create_tag_store_insert_ns,
        tag_store_insert_start
    );
    let alias_on_tag_created_start = rz_profile_start!(profile);
    active_alias_model().on_tag_created(tag, &tmeta);
    rz_profile_add_elapsed_field!(
        profile,
        ref_create_alias_on_tag_created_ns,
        alias_on_tag_created_start
    );
    rz_profile_add_elapsed_field!(profile, ref_create_insert_ns, insert_start);

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
    if rz_trace_tag_create_enabled() {
        eprintln!(
            "[rusteze-runtime][tag-create][ref] tag={} parent={} resolved_parent={} pointee=0x{:x} kind={} bounds={} extent=[0x{:x},+{}] align={} epoch={}",
            tag,
            parent_tag,
            resolved_parent_tag,
            pointee_addr,
            kind_str,
            bounds_len,
            interior_mut_extent_base,
            interior_mut_extent_len,
            align_req,
            alloc_epoch
        );
    }
    tag
}

#[no_mangle]
pub extern "C" fn __record_debug_ref_creation(
    pointee_addr: usize,
    is_mut: u8,
    raw_tag: u64,
    ref_ancestor: u64,
    alias_exempt: u8,
    bounds_len: usize,
    align_req: usize,
) -> u64 {
    let parent_tag = if ref_ancestor != 0 {
        ref_ancestor
    } else {
        raw_tag
    };
    __record_ref_creation(
        pointee_addr,
        is_mut,
        parent_tag,
        alias_exempt,
        bounds_len,
        align_req,
    )
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_raw_ptr_creation"]
pub extern "C" fn __record_raw_ptr_creation(
    pointee_addr: usize,
    is_mut: u8,
    derived_from: u64,
    alias_exempt: u8,
    bounds_len: usize,
    align_req: usize,
) -> u64 {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, raw_create);
    let _g = RzRuntimeGuard::enter();
    let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
    let alias_exempt_flag = (alias_exempt & CREATION_FLAG_ALIAS_EXEMPT) != 0;
    let align_req =
        rz_effective_align_req(align_req, derived_from).max(rz_promised_alignment_for_addr(
            pointee_addr,
            lookup_alloc_snapshot(pointee_addr)
                .map(|(_, meta)| meta.epoch)
                .unwrap_or(0),
        ));
    let propagate_exposed_provenance =
        (alias_exempt & CREATION_FLAG_RAW_PROPAGATE_EXPOSED_PROVENANCE) != 0;
    let projected_helper_parent = (alias_exempt & CREATION_FLAG_PROJECTED_HELPER_PARENT) != 0;
    let carry_bounds_from_source = (alias_exempt & CREATION_FLAG_RAW_CARRY_BOUNDS_FROM_SOURCE) != 0;
    let mut exposed_provenance_root = (alias_exempt & CREATION_FLAG_EXPOSED_PROVENANCE_ROOT) != 0;
    let strict_creation_check = (alias_exempt & CREATION_FLAG_STRICT_RAW_CREATION_CHECK) != 0;
    let deref_creation_check = (alias_exempt & CREATION_FLAG_DEREF_RAW_CREATION) != 0;
    // MIR and optimized std/alloc lowering often materialize administrative `*const`
    // temporaries from mutable-capable sources (e.g. `NonNull`/`Unique` transmute paths)
    // and then write through them. Preserve the parent's effective write capability so
    // these casts do not freeze an otherwise-valid unique/raw-mutable lineage.
    let inherits_write_capability = derived_from != 0
        && projected_helper_parent
        && tag_store::get(derived_from)
            .is_some_and(|parent| matches!(parent.kind, PtrKind::RefMut | PtrKind::RawMut));
    let kind = if is_mut != 0 || inherits_write_capability {
        PtrKind::RawMut
    } else {
        PtrKind::RawConst
    };
    // `alias_exempt` is a creation flag bitfield:
    // - alias-exempt pointee classification
    // - raw exposed-provenance propagation when no tracked parent remains
    // - projected helper parent shape
    // - carry wide bounds from the source pointer when metadata is intentionally dropped
    // - TB-lite raw derived same-family view
    // - exposed-provenance/int-to-ptr root
    // - strict projected/derived raw creation validation
    // - deref-projected raw creation validation
    let mut resolved_parent = derived_from;
    let mut alloc_is_stack = false;
    let mut alloc_size = 0usize;
    let mut parent_alloc_mismatch = false;

    // IMPORTANT: On retagging/derived pointers (derived_from != 0), prefer inheriting the
    // parent's alloc_epoch to keep the original allocation-instance snapshot and make
    // stack-slot reuse detectable as stale pointers. Exception: if the derived pointer
    // clearly points into a different allocation than the parent, refresh to the pointee's
    // allocation epoch (example: `&mut Vec<u8>` on the stack -> `Vec::as_mut_ptr()` heap buffer).
    let (mut alloc_epoch, mut alloc_live_at_creation, mut inherited_bounds_len) =
        if derived_from != 0 {
            let (parent_epoch, parent_live, parent_pointee, inherited_bounds_len) =
                tag_store::get(derived_from)
                    .as_ref()
                    .map(|p| {
                        (
                            p.alloc_epoch,
                            p.alloc_live_at_creation,
                            Some(p.pointee_addr),
                            p.bounds_len,
                        )
                    })
                    .unwrap_or((0, false, None, BOUNDS_LEN_UNKNOWN));

            if let Some(parent_pointee) = parent_pointee {
                let parent_alloc = lookup_alloc_snapshot(parent_pointee);
                let pointee_alloc = lookup_alloc_snapshot(pointee_addr);
                if let Some((pointee_base, pointee_meta)) = pointee_alloc {
                    alloc_is_stack = pointee_meta.is_stack;
                    alloc_size = pointee_meta.size;
                    match parent_alloc {
                        Some((parent_base, _)) if parent_base == pointee_base => {
                            (parent_epoch, parent_live, inherited_bounds_len)
                        }
                        Some((_parent_base, _)) => {
                            parent_alloc_mismatch = true;
                            (pointee_meta.epoch, pointee_meta.live, BOUNDS_LEN_UNKNOWN)
                        }
                        None => {
                            // Parent alloc metadata can be missing in optimized lowering
                            // even when the child pointee alloc is known. Prefer the child
                            // alloc snapshot and allow exact-address lineage recovery below.
                            if parent_pointee != pointee_addr {
                                parent_alloc_mismatch = true;
                            }
                            (pointee_meta.epoch, pointee_meta.live, BOUNDS_LEN_UNKNOWN)
                        }
                    }
                } else {
                    (parent_epoch, parent_live, inherited_bounds_len)
                }
            } else {
                (parent_epoch, parent_live, inherited_bounds_len)
            }
        } else if exposed_provenance_root {
            (0, false, BOUNDS_LEN_UNKNOWN)
        } else {
            // Root creation: if the match is a dead stack slot, treat metadata as unknown
            // to avoid inheriting stale bounds/epoch from recycled stack storage.
            match lookup_alloc_snapshot(pointee_addr) {
                Some((_base, m)) => {
                    alloc_is_stack = m.is_stack;
                    alloc_size = m.size;
                    if m.is_stack && !m.live {
                        (0, false, BOUNDS_LEN_UNKNOWN)
                    } else {
                        (m.epoch, m.live, BOUNDS_LEN_UNKNOWN)
                    }
                }
                None => (0, false, BOUNDS_LEN_UNKNOWN),
            }
        };

    // A strong projected root on a live allocation is a provenance-preserving owner/payload
    // reconstruction. Do not poison it just because an older tag at the same address was exposed.
    let provenance_preserving_alloc_root =
        derived_from == 0 && projected_helper_parent && alloc_epoch != 0 && alloc_live_at_creation;
    if !exposed_provenance_root
        && propagate_exposed_provenance
        && !provenance_preserving_alloc_root
        && pointee_addr != 0
    {
        let resolved_parent_is_tracked = if resolved_parent == 0 {
            false
        } else {
            tag_store::get(resolved_parent).is_some_and(|parent_meta| {
                !rz_has_exposed_provenance_root(resolved_parent, &parent_meta)
            })
        };
        if !resolved_parent_is_tracked {
            let poisoned_same_addr = tags().lock().unwrap().values().any(|meta| {
                meta.pointee_addr == pointee_addr
                    && meta.exposed_provenance_root
                    && (alloc_epoch == 0
                        || meta.alloc_epoch == 0
                        || meta.alloc_epoch == alloc_epoch)
            });
            if poisoned_same_addr {
                exposed_provenance_root = true;
            }
        }
    }

    // `strict_creation_check` is instrumentation's "validate this raw creation eagerly" bit.
    // In strict-provenance mode, exposed-provenance parents are rejected. In default mode, bit7
    // means this is a deref projection (`(*p).field`), so validate allocation reality instead:
    // the parent anchor and projected child must be in the same live allocation.
    let strict_provenance = rz_strict_provenance_enabled();
    if strict_creation_check || (exposed_provenance_root && strict_provenance) {
        if let Some((vk, msg)) = rz_validate_strict_raw_creation_addr(
            pointee_addr,
            kind,
            resolved_parent,
            exposed_provenance_root,
            strict_provenance,
            deref_creation_check,
            if bounds_len_is_known(bounds_len) {
                bounds_len
            } else if carry_bounds_from_source {
                inherited_bounds_len
            } else {
                BOUNDS_LEN_UNKNOWN
            },
        ) {
            rz_violation(vk, append_location_if_enabled(msg, "RZ_LOG_LOC"));
        }
    }

    let bounds_len = if bounds_len_is_known(bounds_len) {
        bounds_len
    } else if carry_bounds_from_source {
        inherited_bounds_len
    } else {
        BOUNDS_LEN_UNKNOWN
    };
    let (interior_mut_extent_base, interior_mut_extent_len) = if resolved_parent != 0 {
        if let Some(parent_meta) = tag_store::get(resolved_parent) {
            (
                parent_meta.interior_mut_extent_base,
                parent_meta.interior_mut_extent_len,
            )
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };
    let (origin_known, origin_base, origin_end) = if exposed_provenance_root {
        (false, 0, 0)
    } else if resolved_parent != 0 {
        tag_store::get(resolved_parent)
            .filter(|parent| parent.origin_known && (!parent_alloc_mismatch || alloc_is_stack))
            .map(|parent| (true, parent.origin_base, parent.origin_end))
            .unwrap_or_else(|| snapshot_tag_origin(pointee_addr, resolved_parent))
    } else {
        snapshot_tag_origin(pointee_addr, resolved_parent)
    };

    let tmeta = TagMeta {
        pointee_addr,
        kind,
        parent: resolved_parent,
        escaped: false,
        alloc_epoch,
        alloc_live_at_creation,
        alias_exempt: alias_exempt_flag,
        lineage_hint: alias_exempt & CREATION_RAW_LINEAGE_HINT_MASK,
        exposed_provenance_root,
        bounds_len,
        interior_mut_extent_base,
        interior_mut_extent_len,
        align_req,
        origin_known,
        origin_base,
        origin_end,
    };
    tag_store::insert(tag, tmeta);
    if tmeta.alloc_epoch != 0 && tmeta.origin_known {
        tag_store::remember_alloc_epoch_tag(tmeta.origin_base, tmeta.alloc_epoch, tag);
    }
    tag_pruning::remember_live_tag(tag, &tmeta);
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
    if rz_trace_tag_create_enabled() {
        eprintln!(
            "[rusteze-runtime][tag-create][raw] tag={} parent={} resolved_parent={} pointee=0x{:x} kind={} bounds={} extent=[0x{:x},+{}] align={} epoch={} hint={}",
            tag,
            derived_from,
            resolved_parent,
            pointee_addr,
            kind_str,
            bounds_len,
            interior_mut_extent_base,
            interior_mut_extent_len,
            align_req,
            alloc_epoch,
            alias_exempt
        );
    }
    tag
}

/// Generic pointer-use event (coarse).
///
/// a pointer value was used/observed, but we did not (yet) classify it as a read or write.
///
/// `addr` is the pointer value (exposed provenance), not an interior offset.
/// `align_req` is the static MIR alignment, and `dyn_ref_slot_addr` is nonzero only for
/// `&dyn Trait` locals whose vtable supplies a stricter dynamic alignment.
#[no_mangle]
pub extern "C" fn __rz_ptr_use(
    tag: u64,
    addr: usize,
    align_req: usize,
    is_mut: u8,
    dyn_ref_slot_addr: usize,
) {
    let profile = rz_profile_context!();
    let _profile_guard = rz_profile_guard!(profile, ptr_use);
    let _g = RzRuntimeGuard::enter();
    let dyn_align = rz_dyn_ref_align_from_slot(dyn_ref_slot_addr);
    let required_ref_align = align_req.max(dyn_align);
    if tag == 0 {
        if dyn_ref_slot_addr != 0 {
            let kind = if is_mut != 0 {
                PtrKind::RefMut
            } else {
                PtrKind::RefShared
            };
            rz_check_tagless_ref_alignment(addr, required_ref_align, kind);
        }
        rz_trace!(
            "[rusteze-runtime] USE: untagged ptr addr=0x{:x} (likely untracked/propagation missing)",
            addr
        );
        return;
    }

    let mut marked_active = false;
    let tmeta_for_use = match tag_store::mark_escaped(tag) {
        Some(tmeta) => {
            marked_active = true;
            Some(tmeta)
        }
        None => tag_store::get(tag),
    };

    if let Some(tmeta) = tmeta_for_use {
        if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut) {
            let promised_align = if bounds_len_is_zero_sized_known(tmeta.bounds_len) {
                0
            } else {
                rz_promised_alignment_for_addr(addr, tmeta.alloc_epoch)
            };
            let required_align = tmeta.align_req.max(promised_align).max(required_ref_align);
            rz_check_alignment(
                "REF_USE",
                tag,
                addr,
                bounds_len_bytes_or_zero(tmeta.bounds_len).max(1),
                required_align,
                required_align,
                Some(&tmeta),
            );
        }
        if marked_active {
            tag_pruning::mark_tag_escaped(tag, &tmeta);
        }
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
        rz_trace!(
            "[rusteze-runtime] USE: unknown tag={} addr=0x{:x}",
            tag,
            addr
        );
    }
}
