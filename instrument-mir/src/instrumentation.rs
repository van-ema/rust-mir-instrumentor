use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

mod metadata_dataflow;

// (rest unchanged)
// NOTE: This pass intentionally avoids instrumenting std/core/alloc directly.
use crate::unsafe_dataflow::{self, UnsafeInfluence, UnsafeSummaryRecord};
use rustc_abi::{FieldIdx, VariantIdx};
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::Mutability;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::{GlobalAlloc, Scalar};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::mir::visit::{MutatingUseContext, NonUseContext, PlaceContext, Visitor};
use rustc_middle::ty::{ConstKind as TyConstKind, GenericArgsRef, Instance, PseudoCanonicalInput, Ty, TyCtxt, TypingEnv};
use rustc_middle::ty::{TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor};
use rustc_middle::ty::TyKind;
use rustc_span::{source_map::Spanned, Span};

pub(crate) struct MyOptimizationPass;

trait FunctionDefId {
    fn func_def_id(&self) -> DefId;
}

impl FunctionDefId for DefId {
    fn func_def_id(&self) -> DefId {
        *self
    }
}

impl<'tcx> FunctionDefId for Instance<'tcx> {
    fn func_def_id(&self) -> DefId {
        self.def_id()
    }
}

struct LocalUseCounter<'a> {
    stats: &'a mut HashMap<Local, LocalRefUseStats>,
}

impl<'a, 'tcx> Visitor<'tcx> for LocalUseCounter<'a> {
    fn visit_place(
        &mut self,
        place: &Place<'tcx>,
        context: PlaceContext,
        location: Location,
    ) {
        let is_def = matches!(
            context,
            PlaceContext::MutatingUse(MutatingUseContext::Store)
                | PlaceContext::MutatingUse(MutatingUseContext::Deinit)
                | PlaceContext::MutatingUse(MutatingUseContext::SetDiscriminant)
                | PlaceContext::MutatingUse(MutatingUseContext::AsmOutput)
                | PlaceContext::MutatingUse(MutatingUseContext::Call)
                | PlaceContext::MutatingUse(MutatingUseContext::Yield)
        );
        if !is_def && !matches!(context, PlaceContext::NonUse(NonUseContext::VarDebugInfo)) {
            self.stats.entry(place.local).or_default().uses += 1;
        }
        self.super_place(place, context, location);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum PassLogLevel {
    Warn,
    Info,
    Trace,
}

#[derive(Default)]
struct UnsafeDflowStats {
    functions_seen: usize,
    functions_enabled: usize,
    ptr_locals_tainted_total: usize,
    ptr_locals_total: usize,
    hooks_total_before: usize,
    hooks_total_after: usize,
    access_hooks_before: usize,
    access_hooks_after: usize,
}

#[derive(Default)]
struct UnsafeCallDflowStats {
    seed_arg_unknown_boundary: usize,
    seed_arg_local_summary_missing: usize,
    seed_arg_summary_direct_sink: usize,
    seed_arg_summary_escape_unknown_direct: usize,
    seed_arg_summary_escape_unknown_inherited: usize,
    seed_arg_raw_fallback: usize,
    backward_dst_unknown_boundary: usize,
    backward_dst_local_summary_missing: usize,
    backward_dst_forward_to_return: usize,
    unknown_callees: std::collections::BTreeMap<String, (usize, usize)>,
}

#[derive(Default)]
struct UnsafeSummaryStats {
    functions_seen: usize,
    functions_with_direct_sink: usize,
    functions_calling_unknown_boundary: usize,
    functions_calling_unknown_boundary_direct: usize,
    functions_calling_unknown_boundary_inherited: usize,
    ptr_args_total: usize,
    ptr_args_with_direct_sink: usize,
    ptr_args_escaping_unknown: usize,
    ptr_args_escaping_unknown_direct: usize,
    ptr_args_escaping_unknown_inherited: usize,
    ptr_args_forwarded_to_return: usize,
}

#[derive(Copy, Clone, Debug, Default)]
struct LocalRefUseStats {
    defs: usize,
    uses: usize,
}


#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AllocShimKind {
    No,
    Alloc,
    AllocZeroed,
    Dealloc,
    Realloc,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CallEffect {
    /// No memory / pointer-tracking relevant effect (e.g., ptr::is_null).
    Ignore,
    /// memcpy/memmove-style (read src, write dst).
    MemCopy,
    /// memset-style (write dst).
    MemSet,
    /// Ptr load/store wrappers/intrinsics (plain or volatile).
    Load,
    Store,
    /// Pointer derivation wrappers that return a pointer derived from arg0 (fresh tag, parent linkage).
    PtrDerive,
    /// Box boundary modeling.
    BoxIntoRaw,
    BoxFromRaw,
    /// Allocator shims/wrappers.
    AllocShim(AllocShimKind),
    /// Not recognized.
    Unknown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MatchKind {
    Contains,
    EndsWith,
}

#[derive(Copy, Clone, Debug)]
struct EffectRule {
    kind1: MatchKind,
    needle1: &'static str,
    // Optional second condition: both must match.
    kind2: Option<MatchKind>,
    needle2: Option<&'static str>,
    effect: CallEffect,
}

impl EffectRule {
    const fn one(kind: MatchKind, needle: &'static str, effect: CallEffect) -> Self {
        Self { kind1: kind, needle1: needle, kind2: None, needle2: None, effect }
    }

    const fn two(
        kind1: MatchKind,
        needle1: &'static str,
        kind2: MatchKind,
        needle2: &'static str,
        effect: CallEffect,
    ) -> Self {
        Self {
            kind1,
            needle1,
            kind2: Some(kind2),
            needle2: Some(needle2),
            effect,
        }
    }
}

fn normalize_def_path(def_path: &str) -> String {
    let mut base = def_path.to_string();
    if let Some(stripped) = strip_trait_impl_prefix(def_path) {
        base = stripped;
    }

    let mut out = String::with_capacity(base.len());
    let mut depth = 0usize;
    let mut colon_run = 0usize;
    for ch in base.chars() {
        match ch {
            '<' => {
                depth += 1;
            }
            '>' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ if depth > 0 => {
                // Skip generic args.
            }
            c => {
                if c.is_whitespace() {
                    continue;
                }
                if c == ':' {
                    colon_run += 1;
                    if colon_run > 2 {
                        continue;
                    }
                } else {
                    colon_run = 0;
                }
                out.push(c);
            }
        }
    }

    out
}

fn strip_trait_impl_prefix(def_path: &str) -> Option<String> {
    if !def_path.starts_with('<') {
        return None;
    }

    let mut depth = 0usize;
    let mut end_idx = None;
    for (idx, ch) in def_path.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = Some(idx);
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    let end_idx = end_idx?;
    let inner = &def_path[1..end_idx];
    let rest = &def_path[(end_idx + 1)..];
    let as_pos = inner.find(" as ")?;
    let trait_part = inner[(as_pos + 4)..].trim();
    if trait_part.is_empty() {
        return None;
    }

    Some(format!("{}{}", trait_part, rest))
}

// Order matters: first match wins.
// These rules cover "simple" std/core wrapper classification that is purely path-string based.
static CALL_EFFECT_RULES: &[EffectRule] = &[
    // ---- Allocator shims & wrappers (order matters) ----

    // Low-level shims.
    EffectRule::one(MatchKind::Contains, "__rust_alloc_zeroed", CallEffect::AllocShim(AllocShimKind::AllocZeroed)),
    EffectRule::one(MatchKind::Contains, "__rust_alloc",        CallEffect::AllocShim(AllocShimKind::Alloc)),
    EffectRule::one(MatchKind::Contains, "__rust_dealloc",      CallEffect::AllocShim(AllocShimKind::Dealloc)),
    EffectRule::one(MatchKind::Contains, "__rust_realloc",      CallEffect::AllocShim(AllocShimKind::Realloc)),

    // alloc::alloc wrappers.
    EffectRule::one(MatchKind::Contains, "alloc::alloc::exchange_malloc", CallEffect::AllocShim(AllocShimKind::Alloc)),
    EffectRule::one(MatchKind::Contains, "alloc::alloc::alloc_zeroed",    CallEffect::AllocShim(AllocShimKind::AllocZeroed)),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(MatchKind::Contains, "alloc::alloc::alloc",           CallEffect::AllocShim(AllocShimKind::Alloc)),
    EffectRule::one(MatchKind::Contains, "alloc::alloc::dealloc",         CallEffect::AllocShim(AllocShimKind::Dealloc)),
    EffectRule::one(MatchKind::Contains, "alloc::alloc::realloc",         CallEffect::AllocShim(AllocShimKind::Realloc)),

    // std::alloc wrappers (often take `Layout`).
    EffectRule::one(MatchKind::Contains, "std::alloc::alloc_zeroed", CallEffect::AllocShim(AllocShimKind::AllocZeroed)),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(MatchKind::Contains, "std::alloc::alloc",        CallEffect::AllocShim(AllocShimKind::Alloc)),
    EffectRule::one(MatchKind::Contains, "std::alloc::dealloc",      CallEffect::AllocShim(AllocShimKind::Dealloc)),
    EffectRule::one(MatchKind::Contains, "std::alloc::realloc",      CallEffect::AllocShim(AllocShimKind::Realloc)),

    // No-op helpers.
    EffectRule::one(MatchKind::EndsWith, "::is_null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::addr_eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null_mut", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::ptr::const_ptr", MatchKind::EndsWith, "::eq", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::ptr::mut_ptr", MatchKind::EndsWith, "::eq", CallEffect::Ignore),

    // ---- Common std/core helpers (suppress unknown-call noise) ----

    // Deref/DerefMut return a reference derived from self.
    EffectRule::two(MatchKind::Contains, "::ops::deref::Deref", MatchKind::EndsWith, "::deref", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ops::deref::DerefMut", MatchKind::EndsWith, "::deref_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ops::Deref", MatchKind::EndsWith, "::deref", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ops::DerefMut", MatchKind::EndsWith, "::deref_mut", CallEffect::PtrDerive),

    // Iterator adaptors: conservative Ignore to avoid treating &mut self as read/write.
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::by_ref", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::for_each", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::size_hint", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::collect", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::next", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::traits::iterator::Iterator", MatchKind::EndsWith, "::nth", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::by_ref", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::for_each", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::size_hint", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::collect", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::next", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::iter::Iterator", MatchKind::EndsWith, "::nth", CallEffect::Ignore),

    // Slice helpers.
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::iter", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::iter_mut", CallEffect::Ignore),

    // ---- PtrDerive wrappers (pointer arithmetic + slice/vec pointer extraction) ----

    // Pointer arithmetic wrappers: constrain to `::ptr::` and method name.
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::add", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::sub", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::offset", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::wrapping_add", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::wrapping_sub", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::wrapping_offset", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::byte_add", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::byte_sub", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::wrapping_byte_add", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::wrapping_byte_sub", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::cast", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::", MatchKind::EndsWith, "::offset_from", CallEffect::Ignore),

    // Slice pointer extraction wrappers.
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::as_ptr", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::as_mut_ptr", CallEffect::PtrDerive),

    // Raw pointer creation helpers.
    EffectRule::one(MatchKind::Contains, "::ptr::from_ref", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::ptr::from_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::NonNull", MatchKind::EndsWith, "::new", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::NonNull", MatchKind::EndsWith, "::new_unchecked", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::NonNull", MatchKind::EndsWith, "::as_ptr", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ptr::NonNull", MatchKind::EndsWith, "::as_mut", CallEffect::PtrDerive),

    // Vec pointer extraction wrappers. Covers `alloc::vec::Vec` and `std::vec::Vec`, including monomorphized forms.
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::as_ptr", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::as_mut_ptr", CallEffect::PtrDerive),

    // Transmute-style helpers returning pointers should preserve lineage.
    EffectRule::one(MatchKind::Contains, "::mem::transmute", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::intrinsics::transmute", CallEffect::PtrDerive),

    // Volatile wrappers (free functions).
    EffectRule::one(MatchKind::Contains, "::ptr::read_volatile", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::ptr::write_volatile", CallEffect::Store),
    // Volatile intrinsics.
    EffectRule::one(MatchKind::Contains, "::intrinsics::volatile_load", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::intrinsics::volatile_store", CallEffect::Store),
    // Core intrinsics used by mem::replace and similar wrappers.
    EffectRule::one(MatchKind::Contains, "::intrinsics::read_via_copy", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::intrinsics::write_via_move", CallEffect::Store),

    // Memset-like.
    EffectRule::one(MatchKind::Contains, "::intrinsics::write_bytes", CallEffect::MemSet),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::write_bytes)
    EffectRule::one(MatchKind::EndsWith, "::write_bytes", CallEffect::MemSet),

    // Plain wrappers.
    // Use suffix matching for `read`/`write` so we don't accidentally match `write_bytes`/`read_bytes`.
    // `rkyv::place::Place<T>` is a write-capability wrapper around an internal raw pointer, not a
    // normal shared-reference write target.
    EffectRule::two(MatchKind::Contains, "rkyv::place::Place", MatchKind::EndsWith, "::write", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "rkyv::place::Place", MatchKind::EndsWith, "::write_unchecked", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "rkyv::place::<impl Place", MatchKind::EndsWith, "::write", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "rkyv::place::<impl Place", MatchKind::EndsWith, "::write_unchecked", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "place::Place", MatchKind::EndsWith, "::write", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "place::Place", MatchKind::EndsWith, "::write_unchecked", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "place::<impl Place", MatchKind::EndsWith, "::write", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "place::<impl Place", MatchKind::EndsWith, "::write_unchecked", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::read_unaligned", CallEffect::Load),
    EffectRule::one(MatchKind::EndsWith, "::read", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::ptr::write_unaligned", CallEffect::Store),
    EffectRule::one(MatchKind::EndsWith, "::write", CallEffect::Store),

    // Memcpy/memmove-like.
    EffectRule::one(MatchKind::Contains, "::intrinsics::copy_nonoverlapping", CallEffect::MemCopy),
    EffectRule::one(MatchKind::Contains, "::intrinsics::copy", CallEffect::MemCopy),
    EffectRule::one(MatchKind::Contains, "::ptr::copy_nonoverlapping", CallEffect::MemCopy),
    EffectRule::one(MatchKind::Contains, "::ptr::copy", CallEffect::MemCopy),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy_nonoverlapping)
    EffectRule::one(MatchKind::EndsWith, "::copy_nonoverlapping", CallEffect::MemCopy),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy)
    EffectRule::one(MatchKind::EndsWith, "::copy", CallEffect::MemCopy),

    // ---- Common pure helpers (Ignore) ----
    EffectRule::two(MatchKind::Contains, "::ops::RangeBounds", MatchKind::EndsWith, "::start_bound", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::ops::RangeBounds", MatchKind::EndsWith, "::end_bound", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::ops::Range", MatchKind::EndsWith, "::contains", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::option::Option", MatchKind::EndsWith, "::expect", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::result::Result", MatchKind::EndsWith, "::is_ok", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::convert::Into", MatchKind::EndsWith, "::into", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::mem::size_of_val", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::mem::take", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::mem::replace", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::panicking::assert_failed", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::fmt::", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::cmp::PartialEq", MatchKind::EndsWith, "::eq", CallEffect::Load),
    EffectRule::two(MatchKind::Contains, "::cmp::PartialOrd", MatchKind::EndsWith, "::partial_cmp", CallEffect::Load),
    EffectRule::two(MatchKind::Contains, "::cmp::Ord", MatchKind::EndsWith, "::cmp", CallEffect::Load),
    EffectRule::two(MatchKind::Contains, "::hash::Hash", MatchKind::EndsWith, "::hash", CallEffect::Load),
    EffectRule::two(MatchKind::Contains, "::hash::impls", MatchKind::EndsWith, "::hash", CallEffect::Load),

    // AsRef/AsMut return borrowed pointers.
    EffectRule::two(MatchKind::Contains, "::convert::AsRef", MatchKind::EndsWith, "::as_ref", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::convert::AsMut", MatchKind::EndsWith, "::as_mut", CallEffect::PtrDerive),

    // Index/IndexMut return references into the receiver.
    EffectRule::two(MatchKind::Contains, "::ops::IndexMut", MatchKind::EndsWith, "::index_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::ops::Index", MatchKind::EndsWith, "::index", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "SliceIndex", MatchKind::EndsWith, "::index_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "SliceIndex", MatchKind::EndsWith, "::index", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::slice::index::<impl", MatchKind::EndsWith, "::index_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::slice::index::<impl", MatchKind::EndsWith, "::index", CallEffect::PtrDerive),

    // Slice helpers.
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::get", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::get_mut", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::last_mut", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::len", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::is_empty", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::split_at", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::split_at_mut", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::copy_from_slice", CallEffect::Ignore),
    // from_raw_parts{,_mut} return slice references derived from the base pointer.
    // This is heavily used by unsafe code; modeling it as PtrDerive avoids losing lineage.
    EffectRule::one(MatchKind::Contains, "::slice::from_raw_parts", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::slice::from_raw_parts_mut", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::slice::raw::from_raw_parts", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::slice::raw::from_raw_parts_mut", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::ptr::slice_from_raw_parts", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::ptr::slice_from_raw_parts_mut", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::iter::IntoIterator", MatchKind::EndsWith, "::into_iter", CallEffect::Ignore),

    // Vec helpers (metadata + length management).
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::len", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::capacity", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::is_empty", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::set_len", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::reserve", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::reserve_exact", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::try_reserve", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::try_reserve_exact", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::extend_from_slice", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::resize", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::from_raw_parts", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::from_raw_parts_in", CallEffect::Ignore),

    EffectRule::two(MatchKind::Contains, "alloc::slice::<impl [", MatchKind::EndsWith, "::to_vec", CallEffect::Ignore),

    // VecDeque helpers.
    EffectRule::two(MatchKind::Contains, "::collections::VecDeque", MatchKind::EndsWith, "::len", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::collections::VecDeque", MatchKind::EndsWith, "::is_empty", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::collections::VecDeque", MatchKind::EndsWith, "::as_slices", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::collections::VecDeque", MatchKind::EndsWith, "::drain", CallEffect::Ignore),

    // String / str helpers.
    EffectRule::two(MatchKind::Contains, "::str::<impl str>", MatchKind::EndsWith, "::len", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::str::<impl str>", MatchKind::EndsWith, "::as_bytes", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::string::String", MatchKind::EndsWith, "::as_bytes", CallEffect::Ignore),

    // IO helpers.
    EffectRule::two(MatchKind::Contains, "::io::Cursor", MatchKind::EndsWith, "::get_ref", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::io::Cursor", MatchKind::EndsWith, "::position", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::io::Cursor", MatchKind::EndsWith, "::set_position", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::io::IoSlice", MatchKind::EndsWith, "::new", CallEffect::Ignore),
    // Pure state queries that only read scalar state from a receiver. These are common in parser
    // and codec hot paths and do not justify materializing a tracked temporary shared borrow.

    // Atomic ops (load/store vs RMW).
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::load", CallEffect::Load),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::store", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::swap", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::compare_exchange", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::compare_exchange_weak", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_add", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_sub", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_and", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_or", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_xor", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_nand", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_max", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::Atomic", MatchKind::EndsWith, "::fetch_min", CallEffect::Store),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::AtomicPtr", MatchKind::EndsWith, "::new", CallEffect::Ignore),
    EffectRule::two(MatchKind::Contains, "::sync::atomic::AtomicPtr", MatchKind::EndsWith, "::get_mut", CallEffect::Ignore),

    EffectRule::one(MatchKind::Contains, "::sync::atomic::atomic_load", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::sync::atomic::atomic_store", CallEffect::Store),
    EffectRule::one(MatchKind::Contains, "::sync::atomic::atomic_compare_exchange", CallEffect::Store),
    EffectRule::one(MatchKind::Contains, "::sync::atomic::atomic_xadd", CallEffect::Store),
    EffectRule::one(MatchKind::Contains, "::sync::atomic::atomic_xsub", CallEffect::Store),

    EffectRule::one(MatchKind::Contains, "::intrinsics::atomic_load", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::intrinsics::atomic_store", CallEffect::Store),
    EffectRule::one(MatchKind::Contains, "::intrinsics::atomic_", CallEffect::Store),

    // Intrinsics + pointer helpers seen in optimized builds.
    EffectRule::one(MatchKind::Contains, "::intrinsics::arith_offset", CallEffect::PtrDerive),
    EffectRule::one(MatchKind::Contains, "::intrinsics::ptr_offset_from", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::intrinsics::ptr_offset_from_unsigned", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::intrinsics::compare_bytes", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::intrinsics::size_of_val", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::intrinsics::align_of_val", CallEffect::Ignore),
];

// Lightweight logging macros for the compiler pass.
// These avoid repeating `if self.log_enabled(...) { eprintln!(...) }`.
macro_rules! rz_pass_log {
    ($pass:expr, $lvl:expr, $($arg:tt)*) => {{
        if ($pass).log_enabled($lvl) {
            eprintln!($($arg)*);
        }
    }};
}

macro_rules! rz_pass_warn {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Warn, $($arg)*)
    };
}

macro_rules! rz_pass_info {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Info, $($arg)*)
    };
}

macro_rules! rz_pass_trace {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Trace, $($arg)*)
    };
}

#[derive(Clone, Debug)]
enum SizeOperand<'tcx> {
    Const(Operand<'tcx>),
    SizeOf(Ty<'tcx>),
    ElemCount { elem_ty: Ty<'tcx>, count_op: Operand<'tcx> },
    /// Size derived from wide-pointer metadata (slice length).
    PtrMetadataSlice { ptr_local: Local, elem_ty: Ty<'tcx> },
    /// Size derived from wide-pointer metadata (str length).
    PtrMetadataStr { ptr_local: Local },
}

#[derive(Clone, Debug)]
enum InstrKind<'tcx> {
    Ref { bk: BorrowKind, src: Place<'tcx> },
    // Raw: created by MIR Rvalue::RawPtr; can propagate a parent tag from the source place.
    // Example MIR: `_p = &raw const (*_r);` where `_r: &u8`.
    Raw { is_mut: bool, src: Place<'tcx> },
    // RawRoot: synthesized for pointer values without a thin-pointer source local
    // (e.g., transmute from NonNull/Unique, projected place, const/global pointer).
    // Example MIR: `_p = transmute::<NonNull<u8>, *const u8>(_nn);`.
    /// Root raw pointer creation for a pointer value already computed in a local.
    /// std/alloc often stores pointers inside ADTs like `NonNull<T>`/`Unique<T>` and then
    /// produces a thin pointer via `Transmute`. Our TagProp only propagates between thin pointer
    /// locals, so without this the destination pointer keeps tag=0 and triggers UNKNOWN_TAG.
    RawRoot { ptr_local: Local, is_mut: bool },
    /// Stack allocation lifetime event for a MIR local.
    StackAlloc { local: Local, live: bool, size_op: SizeOperand<'tcx> },
    /// Heap allocation lifetime event for an allocator-returned pointer.
    /// `ptr_local` holds the pointer value; `size_op` is the allocation size operand (usize).
    HeapAlloc { ptr_local: Local, live: bool, size_op: SizeOperand<'tcx> },
    /// Global/promoted const allocation materialized as a pointer.
    /// `ptr_local` holds the pointer value; `size` is the allocation size (0 = unknown).
    /// `base_offset` is the relative offset of the pointer within the global allocation.
    ConstAlloc { ptr_local: Local, size: usize, base_offset: usize },
    /// Global/promoted const allocation from a constant pointer operand.
    /// Used when the pointer is not stored in a local (e.g., aggregate literals).
    ConstAllocConst { const_op: ConstOperand<'tcx>, size: usize, base_offset: usize },
    /// A write through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrWrite { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// A write through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrWriteAllowUntagged { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// A read through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrRead { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// A read through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrReadAllowUntagged { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// Coarse pointer-use tracking: a pointer-typed local appears in a call argument.
    /// This is treated as an escape event at call boundaries.
    PtrUse { ptr_local: Local },
    /// Propagate tags across pointer-to-pointer casts and plain copies/moves of pointer locals.
    /// This is a local tag assignment, not a runtime hook.
    TagProp {
        dst: Local,
        src: Local,
        copy_tag: bool,
        copy_ref_ancestor: bool,
    },
    /// Fresh tag for a derived pointer value (pointer arithmetic like add/sub/offset).
    /// Emits either ref/raw creation based on destination kind, with `parent=tag(src)`.
    PtrDerive { dst: Local, src: Local, is_mut: bool, is_ref: bool },
    /// Caller-side tag push for pointer arguments to a direct call.
    CallArgPush { callee_id: u64, arg_index: u64, ptr_local: Local },
    /// Callee-side retagging of pointer arguments from the runtime side-channel.
    ArgRetag { callee_id: u64, arg_index: u64, ptr_local: Local },
    /// Callee-side: push the tag for a returned pointer right before `Return`.
    RetPush { callee_id: u64, ptr_local: Local },
    /// Caller-side: take the pushed return tag after a call that returns a pointer.
    RetTake { callee_id: u64, dst_local: Local },
    /// Caller-side: synthesize a fresh tag for an uninstrumented call return.
    RetRoot { dst_local: Local, is_mut: bool, is_ref: bool },
    /// Callee-side: notify runtime alias models that this function is exiting.
    FnExit { callee_id: u64 },
}

#[derive(Clone, Debug)]
struct InsertPoint<'tcx> {
    bb: BasicBlock,
    stmt_idx: usize,
    insert_before: bool,
    source_info: SourceInfo,
    place: Place<'tcx>,
    kind: InstrKind<'tcx>,
}

#[derive(Clone, Debug)]
struct ScanResult<'tcx> {
    insert_points: Vec<InsertPoint<'tcx>>,
    ptr_locals_needing_tag: HashSet<Local>,
}

#[derive(Copy, Clone, Debug)]
struct ConstAllocInfo {
    size: usize,
    base_offset: usize,
}

#[derive(Copy, Clone, Debug)]
struct Hooks {
    def_id_ref: DefId,
    def_id_raw: DefId,
    def_id_alloc: DefId,
    def_id_write: DefId,
    def_id_write_allow_untagged: DefId,
    def_id_read: DefId,
    def_id_read_allow_untagged: DefId,
    def_id_use: DefId,
    def_id_push_call_arg_tag: DefId,
    def_id_take_call_arg_tag: DefId,
    def_id_push_ret_tag: DefId,
    def_id_take_ret_tag_or_root: DefId,
    def_id_exit_fn: DefId,
}

impl MyOptimizationPass {
    fn fn_def_id_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        place: Place<'tcx>,
    ) -> Option<DefId> {
        let ty = place.ty(&body.local_decls, tcx).ty;
        if let TyKind::FnDef(def_id, _) = ty.kind() {
            Some(*def_id)
        } else {
            None
        }
    }

    fn fn_def_id_from_operand<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        op: &Operand<'tcx>,
    ) -> Option<DefId> {
        match op {
            Operand::Constant(c) => self.const_fn_def_id(tcx, body, c),
            Operand::Copy(p) | Operand::Move(p) => self.fn_def_id_from_place(tcx, body, *p),
            _ => None,
        }
    }

    fn backtrack_fn_ptr_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        local: Local,
    ) -> Option<DefId> {
        // Best-effort recovery for calls like:
        //   _f = copy ((*_vtable).0: fn(...));
        //   _0 = _f(args...);
        for stmt in block_data.statements.iter().rev() {
            let StatementKind::Assign(box (dst, rvalue)) = &stmt.kind else {
                continue;
            };
            if dst.as_local() != Some(local) {
                continue;
            }
            let def_id_opt = match rvalue {
                Rvalue::Use(op) => self.fn_def_id_from_operand(tcx, body, op),
                Rvalue::Cast(_, op, _) => self.fn_def_id_from_operand(tcx, body, op),
                Rvalue::CopyForDeref(p) => self.fn_def_id_from_place(tcx, body, *p),
                _ => None,
            };
            if def_id_opt.is_some() {
                return def_id_opt;
            }
        }
        None
    }
    fn log_level(&self) -> PassLogLevel {
        match std::env::var("RZ_LOG")
            .unwrap_or_else(|_| "warn".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "trace" => PassLogLevel::Trace,
            "info" => PassLogLevel::Info,
            _ => PassLogLevel::Warn,
        }
    }

    fn log_enabled(&self, level: PassLogLevel) -> bool {
        self.log_level() >= level
    }

    /// Whether we should suppress coarse PtrUse hooks originating from std/core/alloc.
    /// Default: enabled. Set `RZ_FILTER_STDLIB_USES=0` to disable.
    fn filter_stdlib_uses_enabled(&self) -> bool {
        std::env::var("RZ_FILTER_STDLIB_USES")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    /// Best-effort check: does this span come from the Rust std/core/alloc sources?
    /// This is used to suppress noisy PtrUse hooks for std wrappers (e.g. println!).
    fn span_is_stdlib<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span) -> bool {
        let sm = tcx.sess.source_map();
        let filename = sm.span_to_filename(span);
        // `FileName` is not `Display` on this nightly; use `Debug` formatting.
        let s = format!("{:?}", filename);

        // Matches typical rustup toolchain paths and in-tree paths.
        s.contains("/lib/rustlib/src/rust/library/std/")
            || s.contains("/lib/rustlib/src/rust/library/core/")
            || s.contains("/lib/rustlib/src/rust/library/alloc/")
            || s.contains("/rust/library/std/")
            || s.contains("/rust/library/core/")
            || s.contains("/rust/library/alloc/")
            || s.contains("/rust/library/proc_macro/")
            || s.contains("/library/std/")
            || s.contains("/library/core/")
            || s.contains("/library/alloc/")
    }
    /// Return true for any pointer or reference type, including wide pointers like slices and str.
    /// We treat these as tag-carrying so that when MIR later extracts a thin data pointer, the
    /// original tag can be propagated instead of silently dropping to tag zero.
    fn is_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
    }

    fn is_raw_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::RawPtr(..))
    }

    /// Best-effort detection of "vtable-like" structs: all fields are function pointers.
    fn is_fn_table_adt_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        let TyKind::Adt(adt, args) = ty.kind() else { return false };
        if !adt.is_struct() {
            return false;
        }
        let variant = adt.non_enum_variant();
        if variant.fields.is_empty() {
            return false;
        }

        for field in variant.fields.iter() {
            let fty = field.ty(tcx, args);
            match fty.kind() {
                TyKind::FnPtr(..) | TyKind::FnDef(..) => {}
                _ => return false,
            }
        }
        true
    }

    /// Best-effort detection of "vtable-like" pointers: pointers to structs whose
    /// fields are all function pointers. These typically live in static memory.
    fn is_vtable_like_ptr_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        let pointee = match ty.kind() {
            TyKind::Ref(_, p, _) | TyKind::RawPtr(p, _) => *p,
            _ => return false,
        };
        self.is_fn_table_adt_ty(tcx, pointee)
    }

    /// Return true only for *thin* pointers (one machine word).
    ///
    /// IMPORTANT: do **not** call `tcx.layout_of` / `layout_size_bytes` here.
    /// During MIR instrumentation we may see generic/projection types that cannot be
    /// normalized yet (e.g. `&[<I as Iterator>::Item; 0]` inside `SmallVec`), and forcing a
    /// layout query can surface an `E0080` "unable to determine layout ... cannot be normalized"
    /// error during compilation.
    ///
    /// Instead, classify fat pointers syntactically by looking at the pointee type:
    /// references/raw-pointers to DSTs (`[T]`, `str`, `dyn Trait`) are fat; everything else is
    /// treated as thin.
    fn is_thin_ptr_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                // Be conservative for unresolved/generic pointees: if we misclassify a fat pointer
                // as thin, `PointerExposeProvenance` on the pair-typed value can ICE during codegen.
                if pointee.has_param()
                    || pointee.has_infer()
                    || pointee.has_aliases()
                    || pointee.has_opaque_types()
                    || pointee.has_placeholders()
                    || pointee.has_bound_vars()
                {
                    return false;
                }

                match pointee.kind() {
                    TyKind::Slice(..) | TyKind::Str | TyKind::Dynamic(..) => false,
                    // `extern type` is unsized but uses `()` metadata, so pointers are thin.
                    TyKind::Foreign(..) => true,
                    _ => pointee.is_sized(tcx, body.typing_env(tcx)),
                }
            }
            _ => false,
        }
    }

    /// Return true when we can safely extract a concrete address from a pointer type.
    /// This is stricter than `is_thin_ptr_ty`: we also require the pointee to be sized
    /// in the current typing environment.
    fn is_addr_exposable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                self.is_thin_ptr_ty(tcx, body, ty) && pointee.is_sized(tcx, body.typing_env(tcx))
            }
            _ => false,
        }
    }

    /// Return true when call-boundary return tagging is safe and useful for this pointer type.
    ///
    /// We always include thin pointers. For wide pointers, we currently include slice/str
    /// pointers (metadata is a length and we can derive byte bounds precisely), but skip
    /// `dyn Trait`/other DST metadata forms to avoid conservative false positives.
    fn supports_call_boundary_ret_tag_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if self.is_addr_exposable_ptr_ty(tcx, body, ty) {
                    return true;
                }
                matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str)
            }
            _ => false,
        }
    }

    /// Produce a thin raw pointer type suitable for extracting the data pointer from a wide pointer.
    /// We only care about the address, so a pointer to unit keeps the correct size and mutability
    /// while discarding the metadata.
    fn data_ptr_ty_for_ptr<'tcx>(&self, tcx: TyCtxt<'tcx>, ptr_ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
        match ptr_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) | TyKind::RawPtr(_ty, mutbl) => {
                let is_mut = matches!(mutbl, Mutability::Mut);
                Some(if is_mut {
                    Ty::new_mut_ptr(tcx, tcx.types.unit)
                } else {
                    Ty::new_imm_ptr(tcx, tcx.types.unit)
                })
            }
            _ => None,
        }
    }

    /// Extract mutability from a raw pointer or reference type.
    fn ptr_is_mut<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        }
    }

    /// Ensure `ptr_local` has a tag by synthesizing a `RawRoot` before the current statement
    /// if it hasn't been tagged yet.
    ///
    /// For wide pointers (`&[T]`, `&str`, `dyn Trait`), RawRoot lowering will first extract the
    /// thin data pointer (dropping metadata) and root-tag that address.
    fn ensure_raw_root_before<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        ptr_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
    ) {
        if tagged_ptr_locals.contains(&ptr_local) || ptr_locals_with_tag_sources.contains(&ptr_local) {
            return;
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_pointer_ty(ptr_ty) {
            return;
        }

        let is_mut = self.ptr_is_mut(ptr_ty);
        tagged_ptr_locals.insert(ptr_local);
        ptr_locals_needing_tag.insert(ptr_local);
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before: true,
            source_info,
            place: Place::from(ptr_local),
            kind: InstrKind::RawRoot { ptr_local, is_mut },
        });
    }

    /// Pre-scan the body to find pointer locals that are assigned from a known pointer source.
    /// This prevents later RawRoot insertion from overwriting tags when control-flow order
    /// differs from basic-block index order.
    fn collect_ptr_locals_with_tag_sources<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> HashSet<Local> {
        let mut locals = HashSet::new();

        for block_data in body.basic_blocks.iter() {
            for stmt in block_data.statements.iter() {
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else { continue };
                let Some(dst_local) = dst_place.as_local() else { continue };
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(..) | Rvalue::RawPtr(..) => {
                        locals.insert(dst_local);
                    }
                    Rvalue::Use(op) => {
                        if let Some(src_local) = self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    Rvalue::CopyForDeref(p) => {
                        if let Some(src_local) = p.as_local() {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _,
                    ) => {
                        if let Some(src_local) = self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                        if let Some(src_local) = self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) || src_ty.is_integral() {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    _ => {}
                }
            }

        }

        locals
    }

    /// Build statements that compute `addr_local` from a pointer-typed place.
    ///
    /// - For thin pointers we can `PointerExposeProvenance` directly.
    /// - For wide pointers (`&[T]`, `&str`, `dyn Trait`), we first cast to a thin raw pointer
    ///   to unit (`*const ()` / `*mut ()`) to drop metadata, then expose provenance from the
    ///   thin data pointer.
    fn addr_stmts_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
    ) -> Option<(Option<Statement<'tcx>>, Statement<'tcx>)> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_thin_ptr_ty(tcx, body, place_ty) {
            let addr_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(addr_local),
                    Rvalue::Cast(
                        CastKind::PointerExposeProvenance,
                        Operand::Copy(place),
                        tcx.types.usize,
                    ),
                ))),
            );
            return Some((None, addr_stmt));
        }

        // Wide pointer: extract data pointer first.
        let data_ptr_ty = self.data_ptr_ty_for_ptr(tcx, place_ty)?;
        let data_ptr_local = body
            .local_decls
            .push(LocalDecl::new(data_ptr_ty, source_info.span));

        let data_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(data_ptr_local),
                Rvalue::Cast(CastKind::PtrToPtr, Operand::Copy(place), data_ptr_ty),
            ))),
        );

        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(data_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((Some(data_ptr_stmt), addr_stmt))
    }

    /// Whether to warn about unknown (unclassified) direct calls that may read/write memory via pointers.
    /// Default: enabled. Set `RZ_WARN_UNKNOWN_CALLS=0` to disable.
    fn warn_unknown_calls_enabled(&self) -> bool {
        std::env::var("RZ_WARN_UNKNOWN_CALLS")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    /// If true, emit MIR-based heap alloc/free hooks (`HeapAlloc` / `__rz_record_alloc`).
    /// Default: false (we rely on the runtime's global allocator wrapper in `runtime/src/lib.rs`).
    /// Set `RZ_HEAP_ALLOCS_FROM_MIR=1` to force the old behavior.
    fn heap_allocs_from_mir_enabled(&self) -> bool {
        std::env::var("RZ_HEAP_ALLOCS_FROM_MIR")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    /// Caller-side return-tag recovery is always enabled.
    ///
    /// We keep this as a helper to make call-boundary policy explicit in one place.
    fn ret_take_enabled(&self) -> bool {
        true
    }

    /// Callee-side return-tag push is always enabled.
    ///
    /// Together with `ret_take_enabled`, this keeps return-pointer provenance connected
    /// across instrumented call boundaries by default.
    fn ret_push_enabled(&self) -> bool {
        true
    }

    /// Whether to emit stack-dead events on `StorageDead`.
    ///
    /// Default: disabled, because optimized MIR can place `StorageDead` before
    /// a final use through outstanding references. Enable for experiments with:
    /// `RZ_USE_STORAGE_DEAD=1`.
    fn use_storage_dead_enabled(&self) -> bool {
        std::env::var("RZ_USE_STORAGE_DEAD")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    /// Print every emitted stack allocation/deallocation hook.
    ///
    /// Default: disabled. Enable with `RZ_TRACE_STACK_ALLOCS=1`.
    fn trace_stack_allocs_enabled(&self) -> bool {
        std::env::var("RZ_TRACE_STACK_ALLOCS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    /// Enable crate-local unsafe-influence dataflow gating for pointer access hooks.
    ///
    /// Supported modes:
    /// - analyze-only summary generation (`RZ_ANALYZE_UNSAFE_SUMMARIES=1`)
    /// - final builds consuming merged summaries (`RZ_USE_UNSAFE_SUMMARIES=1`)
    ///
    /// Plain local-only `RZ_UNSAFE_DATAFLOW=1` pruning is intentionally not supported as a
    /// user-facing mode because it is not sound enough for the full example suite.
    fn unsafe_dataflow_selective_enabled(&self) -> bool {
        unsafe_dataflow::unsafe_dataflow_enabled()
    }

    fn unsafe_dataflow_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn unsafe_dataflow_summary_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn unsafe_dataflow_summary_dump_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn unsafe_dataflow_call_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_CALL_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn unsafe_dataflow_unknown_callee_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_UNKNOWN_CALLEE_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn analyze_unsafe_summaries_only_enabled(&self) -> bool {
        unsafe_dataflow::analyze_unsafe_summaries_enabled()
    }

    fn trace_unsafe_dataflow_enabled(&self) -> bool {
        std::env::var("RZ_TRACE_UNSAFE_DATAFLOW")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn unsafe_dataflow_gated_local<'tcx>(
        kind: &InstrKind<'tcx>,
        place: &Place<'tcx>,
    ) -> Option<Local> {
        match kind {
            InstrKind::PtrRead { ptr_local, .. }
            | InstrKind::PtrWrite { ptr_local, .. }
            | InstrKind::PtrReadAllowUntagged { ptr_local, .. }
            | InstrKind::PtrWriteAllowUntagged { ptr_local, .. }
            | InstrKind::PtrUse { ptr_local }
            | InstrKind::RawRoot { ptr_local, .. }
            | InstrKind::RetRoot { dst_local: ptr_local, .. }
            | InstrKind::PtrDerive { dst: ptr_local, .. } => Some(*ptr_local),
            InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
            _ => None,
        }
    }

    fn log_unsafe_dataflow_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
        hooks_total_before: usize,
        hooks_total_after: usize,
        access_hooks_before: usize,
        access_hooks_after: usize,
    ) {
        if !self.unsafe_dataflow_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let access_dropped = access_hooks_before.saturating_sub(access_hooks_after);
        let total_dropped = hooks_total_before.saturating_sub(hooks_total_after);
        eprintln!(
            "[rusteze][unsafe-dflow][fn] crate={} fn={} enabled={} tainted_ptrs={} total_ptrs={} access_hooks {}->{} dropped={} total_hooks {}->{} dropped={}",
            crate_name,
            fn_name,
            unsafe_influence.enabled(),
            unsafe_influence.tainted_ptr_count(),
            unsafe_influence.total_ptr_count(),
            access_hooks_before,
            access_hooks_after,
            access_dropped,
            hooks_total_before,
            hooks_total_after,
            total_dropped
        );

        static STATS: OnceLock<Mutex<UnsafeDflowStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeDflowStats::default()))
            .lock()
            .unwrap();

        stats.functions_seen += 1;
        if unsafe_influence.enabled() {
            stats.functions_enabled += 1;
        }
        stats.ptr_locals_tainted_total += unsafe_influence.tainted_ptr_count();
        stats.ptr_locals_total += unsafe_influence.total_ptr_count();
        stats.hooks_total_before += hooks_total_before;
        stats.hooks_total_after += hooks_total_after;
        stats.access_hooks_before += access_hooks_before;
        stats.access_hooks_after += access_hooks_after;

        eprintln!(
            "[rusteze][unsafe-dflow][totals] crate={} fns={} enabled_fns={} ptr_locals tainted/total={}/{} access_hooks {}->{} dropped={} total_hooks {}->{} dropped={}",
            crate_name,
            stats.functions_seen,
            stats.functions_enabled,
            stats.ptr_locals_tainted_total,
            stats.ptr_locals_total,
            stats.access_hooks_before,
            stats.access_hooks_after,
            stats.access_hooks_before.saturating_sub(stats.access_hooks_after),
            stats.hooks_total_before,
            stats.hooks_total_after,
            stats.hooks_total_before.saturating_sub(stats.hooks_total_after)
        );
    }

    fn log_unsafe_dataflow_call_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_call_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let call_stats = unsafe_influence.call_stats();

        eprintln!(
            "[rusteze][unsafe-call][fn] crate={} fn={} seed_arg_unknown={} seed_arg_local_missing={} seed_arg_direct_sink={} seed_arg_escape_unknown_direct={} seed_arg_escape_unknown_inherited={} seed_arg_raw_fallback={} backward_dst_unknown={} backward_dst_local_missing={} backward_dst_forward_to_return={}",
            crate_name,
            fn_name,
            call_stats.seed_arg_unknown_boundary,
            call_stats.seed_arg_local_summary_missing,
            call_stats.seed_arg_summary_direct_sink,
            call_stats.seed_arg_summary_escape_unknown_direct,
            call_stats.seed_arg_summary_escape_unknown_inherited,
            call_stats.seed_arg_raw_fallback,
            call_stats.backward_dst_unknown_boundary,
            call_stats.backward_dst_local_summary_missing,
            call_stats.backward_dst_forward_to_return,
        );

        static STATS: OnceLock<Mutex<UnsafeCallDflowStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeCallDflowStats::default()))
            .lock()
            .unwrap();

        stats.seed_arg_unknown_boundary += call_stats.seed_arg_unknown_boundary;
        stats.seed_arg_local_summary_missing += call_stats.seed_arg_local_summary_missing;
        stats.seed_arg_summary_direct_sink += call_stats.seed_arg_summary_direct_sink;
        stats.seed_arg_summary_escape_unknown_direct +=
            call_stats.seed_arg_summary_escape_unknown_direct;
        stats.seed_arg_summary_escape_unknown_inherited +=
            call_stats.seed_arg_summary_escape_unknown_inherited;
        stats.seed_arg_raw_fallback += call_stats.seed_arg_raw_fallback;
        stats.backward_dst_unknown_boundary += call_stats.backward_dst_unknown_boundary;
        stats.backward_dst_local_summary_missing += call_stats.backward_dst_local_summary_missing;
        stats.backward_dst_forward_to_return += call_stats.backward_dst_forward_to_return;
        for (callee, counts) in &call_stats.unknown_callees {
            let entry = stats.unknown_callees.entry(callee.clone()).or_default();
            entry.0 += counts.seed_arg_unknown_boundary;
            entry.1 += counts.backward_dst_unknown_boundary;
        }

        eprintln!(
            "[rusteze][unsafe-call][totals] crate={} seed_arg_unknown={} seed_arg_local_missing={} seed_arg_direct_sink={} seed_arg_escape_unknown_direct={} seed_arg_escape_unknown_inherited={} seed_arg_raw_fallback={} backward_dst_unknown={} backward_dst_local_missing={} backward_dst_forward_to_return={}",
            crate_name,
            stats.seed_arg_unknown_boundary,
            stats.seed_arg_local_summary_missing,
            stats.seed_arg_summary_direct_sink,
            stats.seed_arg_summary_escape_unknown_direct,
            stats.seed_arg_summary_escape_unknown_inherited,
            stats.seed_arg_raw_fallback,
            stats.backward_dst_unknown_boundary,
            stats.backward_dst_local_summary_missing,
            stats.backward_dst_forward_to_return,
        );
        if self.unsafe_dataflow_unknown_callee_stats_enabled() {
            for (callee, (seed_unknown, backward_unknown)) in stats
                .unknown_callees
                .iter()
                .filter(|(_, counts)| counts.0 != 0 || counts.1 != 0)
            {
                eprintln!(
                    "[rusteze][unsafe-call][unknown] crate={} callee={} seed_arg_unknown={} backward_dst_unknown={}",
                    crate_name,
                    callee,
                    seed_unknown,
                    backward_unknown,
                );
            }
        }
    }

    fn log_unsafe_dataflow_summary_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_summary_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let summary = unsafe_influence.summary();
        let ptr_args_total = summary.ptr_args().len();
        let ptr_args_with_direct_sink = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.reaches_direct_sink())
            .count();
        let ptr_args_escaping_unknown = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_unknown_boundary())
            .count();
        let ptr_args_escaping_unknown_direct = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_direct_unknown_boundary())
            .count();
        let ptr_args_escaping_unknown_inherited = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_inherited_unknown_boundary())
            .count();
        let ptr_args_forwarded_to_return = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.forwarded_to_return())
            .count();

        eprintln!(
            "[rusteze][unsafe-summary][fn] crate={} fn={} direct_sink={} calls_unknown_boundary={} direct_unknown={} inherited_unknown={} ptr_args={} direct_sink_args={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
            crate_name,
            fn_name,
            summary.has_direct_sink(),
            summary.calls_unknown_boundary(),
            summary.calls_unknown_boundary_direct(),
            summary.calls_unknown_boundary_inherited(),
            ptr_args_total,
            ptr_args_with_direct_sink,
            ptr_args_escaping_unknown,
            ptr_args_escaping_unknown_direct,
            ptr_args_escaping_unknown_inherited,
            ptr_args_forwarded_to_return
        );

        for arg in summary.ptr_args() {
            eprintln!(
                "[rusteze][unsafe-summary][arg] crate={} fn={} arg_index={} direct_sink_mask=0x{:x} propagation_mask=0x{:x} direct_sink={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
                crate_name,
                fn_name,
                arg.arg_index,
                arg.direct_sink_mask,
                arg.propagation_mask,
                arg.reaches_direct_sink(),
                arg.escapes_to_unknown_boundary(),
                arg.escapes_to_direct_unknown_boundary(),
                arg.escapes_to_inherited_unknown_boundary(),
                arg.forwarded_to_return()
            );
        }

        static STATS: OnceLock<Mutex<UnsafeSummaryStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeSummaryStats::default()))
            .lock()
            .unwrap();

        stats.functions_seen += 1;
        stats.functions_with_direct_sink += usize::from(summary.has_direct_sink());
        stats.functions_calling_unknown_boundary += usize::from(summary.calls_unknown_boundary());
        stats.functions_calling_unknown_boundary_direct +=
            usize::from(summary.calls_unknown_boundary_direct());
        stats.functions_calling_unknown_boundary_inherited +=
            usize::from(summary.calls_unknown_boundary_inherited());
        stats.ptr_args_total += ptr_args_total;
        stats.ptr_args_with_direct_sink += ptr_args_with_direct_sink;
        stats.ptr_args_escaping_unknown += ptr_args_escaping_unknown;
        stats.ptr_args_escaping_unknown_direct += ptr_args_escaping_unknown_direct;
        stats.ptr_args_escaping_unknown_inherited += ptr_args_escaping_unknown_inherited;
        stats.ptr_args_forwarded_to_return += ptr_args_forwarded_to_return;

        eprintln!(
            "[rusteze][unsafe-summary][totals] crate={} fns={} direct_sink_fns={} calls_unknown_boundary_fns={} direct_unknown_fns={} inherited_unknown_fns={} ptr_args={} direct_sink_args={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
            crate_name,
            stats.functions_seen,
            stats.functions_with_direct_sink,
            stats.functions_calling_unknown_boundary,
            stats.functions_calling_unknown_boundary_direct,
            stats.functions_calling_unknown_boundary_inherited,
            stats.ptr_args_total,
            stats.ptr_args_with_direct_sink,
            stats.ptr_args_escaping_unknown,
            stats.ptr_args_escaping_unknown_direct,
            stats.ptr_args_escaping_unknown_inherited,
            stats.ptr_args_forwarded_to_return
        );
    }

    fn unsafe_summary_dump_path<'tcx>(&self, tcx: TyCtxt<'tcx>) -> PathBuf {
        let crate_name = tcx.crate_name(LOCAL_CRATE).as_str().replace('-', "_");
        if let Ok(path) = std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP_PATH") {
            return PathBuf::from(path);
        }
        let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
        PathBuf::from(target_dir)
            .join("rusteze-unsafe-summaries")
            .join(format!("{crate_name}.jsonl"))
    }

    fn dump_unsafe_dataflow_summary<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_summary_dump_enabled() {
            return;
        }

        let path = self.unsafe_summary_dump_path(tcx);
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                rz_pass_warn!(
                    self,
                    "[rusteze][unsafe-summary] failed to create dump dir {}: {}",
                    parent.display(),
                    err
                );
                return;
            }
        }

        let fn_name = tcx.def_path_str(body.source.def_id());
        let fn_hash = {
            let hash = tcx.def_path_hash(body.source.def_id());
            format!("{:x}:{:x}", hash.stable_crate_id(), hash.local_hash())
        };
        let (trait_fn_name, trait_fn_hash) = tcx
            .opt_associated_item(body.source.def_id())
            .and_then(|item| item.trait_item_def_id)
            .filter(|trait_did| *trait_did != body.source.def_id())
            .map(|trait_did| {
                let hash = tcx.def_path_hash(trait_did);
                (
                    tcx.def_path_str(trait_did),
                    format!("{:x}:{:x}", hash.stable_crate_id(), hash.local_hash()),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new()));
        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let summary = unsafe_influence.summary();
        let record = UnsafeSummaryRecord {
            crate_name: crate_name.to_string(),
            function: fn_name,
            function_hash: fn_hash,
            trait_function: trait_fn_name,
            trait_function_hash: trait_fn_hash,
            has_direct_sink: summary.has_direct_sink(),
            calls_unknown_boundary: summary.calls_unknown_boundary(),
            calls_unknown_boundary_direct: summary.calls_unknown_boundary_direct(),
            calls_unknown_boundary_inherited: summary.calls_unknown_boundary_inherited(),
            ptr_args: summary.ptr_args().to_vec(),
            local_callsites: unsafe_influence.local_callsites().to_vec(),
        };
        let Ok(mut line) = serde_json::to_string(&record) else {
            rz_pass_warn!(self, "[rusteze][unsafe-summary] failed to serialize summary");
            return;
        };
        line.push('\n');

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                if let Err(err) = file.write_all(line.as_bytes()) {
                    rz_pass_warn!(
                        self,
                        "[rusteze][unsafe-summary] failed to write {}: {}",
                        path.display(),
                        err
                    );
                }
            }
            Err(err) => {
                rz_pass_warn!(
                    self,
                    "[rusteze][unsafe-summary] failed to open {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    fn filter_insert_points_by_unsafe_dataflow<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        mut insert_points: Vec<InsertPoint<'tcx>>,
        unsafe_influence: &UnsafeInfluence,
    ) -> Vec<InsertPoint<'tcx>> {
        let before_total = insert_points.len();
        let before_access = insert_points
            .iter()
            .filter(|ip| {
                matches!(
                    ip.kind,
                    InstrKind::PtrRead { .. }
                        | InstrKind::PtrWrite { .. }
                        | InstrKind::PtrReadAllowUntagged { .. }
                        | InstrKind::PtrWriteAllowUntagged { .. }
                        | InstrKind::PtrUse { .. }
                )
            })
            .count();

        if !unsafe_influence.enabled() {
            self.log_unsafe_dataflow_stats(
                tcx,
                body,
                unsafe_influence,
                before_total,
                before_total,
                before_access,
                before_access,
            );
            self.log_unsafe_dataflow_call_stats(tcx, body, unsafe_influence);
            return insert_points;
        }

        insert_points.retain(|ip| {
            if !matches!(
                ip.kind,
                InstrKind::PtrRead { .. }
                    | InstrKind::PtrWrite { .. }
                    | InstrKind::PtrReadAllowUntagged { .. }
                    | InstrKind::PtrWriteAllowUntagged { .. }
            ) {
                return true;
            }
            let ptr_local_opt = Self::unsafe_dataflow_gated_local(&ip.kind, &ip.place);
            ptr_local_opt
                .filter(|&l| self.is_raw_pointer_ty(body.local_decls[l].ty))
                .map(|l| unsafe_influence.should_instrument_ptr_local(l))
                .unwrap_or(true)
        });

        let after_total = insert_points.len();
        let after_access = insert_points
            .iter()
            .filter(|ip| {
                matches!(
                    ip.kind,
                    InstrKind::PtrRead { .. }
                        | InstrKind::PtrWrite { .. }
                        | InstrKind::PtrReadAllowUntagged { .. }
                        | InstrKind::PtrWriteAllowUntagged { .. }
                        | InstrKind::PtrUse { .. }
                )
            })
            .count();

        if self.trace_unsafe_dataflow_enabled() {
            let dropped = before_total.saturating_sub(after_total);
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-dflow] filtered {} access hooks (kept {} / tainted_ptrs={} total_ptrs={})",
                dropped,
                after_total,
                unsafe_influence.tainted_ptr_count(),
                unsafe_influence.total_ptr_count()
            );
        }

        self.log_unsafe_dataflow_stats(
            tcx,
            body,
            unsafe_influence,
            before_total,
            after_total,
            before_access,
            after_access,
        );
        self.log_unsafe_dataflow_call_stats(tcx, body, unsafe_influence);

        insert_points
    }

    fn trace_stack_alloc_emit<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
        live: bool,
        size_op: &SizeOperand<'tcx>,
        source: &str,
    ) {
        if !self.trace_stack_allocs_enabled() {
            return;
        }
        let def_path = tcx.def_path_str(body.source.def_id());
        rz_pass_warn!(
            self,
            "[rusteze][trace-stack] fn={} local=_{} live={} size_op={:?} source={}",
            def_path,
            local.index(),
            live,
            size_op,
            source
        );
    }

    /// Print an "unknown call" warning once per callee def-path to avoid spam.
    fn warn_unknown_call_once(&self, def_path: &str) {
        static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut guard = set.lock().unwrap();
        if guard.insert(def_path.to_string()) {
            if self.log_enabled(PassLogLevel::Trace) {
                static TRACE_UNKNOWN: OnceLock<Mutex<usize>> = OnceLock::new();
                let mut count = TRACE_UNKNOWN.get_or_init(|| Mutex::new(0)).lock().unwrap();
                if *count < 10 {
                    *count += 1;
                    rz_pass_warn!(
                        self,
                        "[rusteze][trace] unknown_call def_path={:?} contains_slice_impl={} ends_get={} ends_get_mut={} ends_is_empty={}",
                        def_path,
                        def_path.contains("::slice::<impl ["),
                        def_path.ends_with("::get"),
                        def_path.ends_with("::get_mut"),
                        def_path.ends_with("::is_empty")
                    );
                }
            }
            if std::env::var("RZ_TRACE_UNKNOWN_CALLS")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
            {
                static TRACE_UNKNOWN_DETAILS: OnceLock<Mutex<usize>> = OnceLock::new();
                let mut count = TRACE_UNKNOWN_DETAILS
                    .get_or_init(|| Mutex::new(0))
                    .lock()
                    .unwrap();
                if *count < 20 {
                    *count += 1;
                    rz_pass_warn!(
                        self,
                        "[rusteze][trace] unknown_call_details def_path={:?} bytes={:?} contains_fmt={} contains_slice_impl={} contains_vec={} contains_index={} ends_index={} ends_index_mut={}",
                        def_path,
                        def_path.as_bytes(),
                        def_path.contains("::fmt::"),
                        def_path.contains("::slice::<impl ["),
                        def_path.contains("::vec::Vec"),
                        def_path.contains("::ops::Index"),
                        def_path.ends_with("::index"),
                        def_path.ends_with("::index_mut")
                    );
                }
            }
            rz_pass_warn!(
                self,
                "[rusteze][warn] unclassified direct call with pointer effects: {} (consider adding a wrapper/intrinsic classifier or instrumenting that crate)",
                def_path
            );
        }
    }

    fn callee_id_u64<'tcx>(&self, tcx: TyCtxt<'tcx>, def_id: DefId) -> u64 {
        tcx.def_path_hash(def_id)
            .0
            .to_smaller_hash()
            .as_u64()
    }

    fn parse_instrumented_crates_env(&self) -> Option<HashSet<String>> {
        let raw = std::env::var("RZ_INSTRUMENTED_CRATES").ok()?;
        let mut set = HashSet::new();
        for part in raw.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            // Allow either '-' or '_' in names; rustc uses '_' for crate_name().
            set.insert(p.replace('-', "_"));
        }
        Some(set)
    }

    fn is_std_like_crate_name(&self, name: &str) -> bool {
        matches!(name, "core" | "std")
    }

    fn instrumented_crates_cached<'tcx>(&self, tcx: TyCtxt<'tcx>) -> &'static HashSet<String> {
        static INSTRUMENTED: OnceLock<HashSet<String>> = OnceLock::new();

        INSTRUMENTED.get_or_init(|| {
            // Priority 1: explicit allowlist
            if let Some(env_set) = self.parse_instrumented_crates_env() {
                return env_set;
            }

            // Priority 2: instrument all non-runtime dependencies
            let instrument_all_deps = std::env::var("RZ_INSTRUMENT_ALL_DEPS")
                .ok()
                .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if !instrument_all_deps {
                return HashSet::new();
            }

            let mut set = HashSet::new();
            for &cnum in tcx.crates(()).iter() {
                let name = tcx.crate_name(cnum).as_str().to_string();
                if name == "runtime" {
                    continue;
                }

                // Even in "instrument all deps" mode, do NOT treat std/core as instrumented
                // callees. We rely on wrapper classification there.
                if self.is_std_like_crate_name(&name) {
                    continue;
                }

                set.insert(name);
            }
            set
        })
    }

    fn maybe_print_crate_graph<'tcx>(&self, tcx: TyCtxt<'tcx>) {
        static PRINTED: OnceLock<()> = OnceLock::new();
        if PRINTED.get().is_some() {
            return;
        }

        let print = std::env::var("RZ_PRINT_CRATES")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

        if !print {
            return;
        }

        // Mark as printed once.
        let _ = PRINTED.set(());

        let allow = self.instrumented_crates_cached(tcx);
        eprintln!("[rusteze] crates in compilation graph:");
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum).as_str().to_string();
            let flag = if allow.contains(&name) { "instrumented" } else { "dep" };
            eprintln!("  - {} ({})", name, flag);
        }
        eprintln!("[rusteze] note: current crate is always treated as instrumented; set RZ_INSTRUMENTED_CRATES or RZ_INSTRUMENT_ALL_DEPS=1 to include dependencies.");
    }

    fn is_instrumented_callee<'tcx>(&self, tcx: TyCtxt<'tcx>, def_id: DefId) -> bool {
        // Optionally print the crate graph once per compilation.
        self.maybe_print_crate_graph(tcx);

        // Never consider the runtime crate instrumented (avoid recursion).
        let crate_name_sym = tcx.crate_name(def_id.krate);
        let crate_name = crate_name_sym.as_str();
        if crate_name == "runtime" {
            return false;
        }
        if crate_name == "core" || crate_name == "std" {
            return false;
        }

        // Always treat the local crate as instrumented.
        if def_id.krate == LOCAL_CRATE {
            return true;
        }

        // "Instrument all deps" mode is now the default (unless explicitly disabled).
        let instrument_all_deps = std::env::var("RZ_INSTRUMENT_ALL_DEPS")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false");
        if instrument_all_deps {
            return true;
        }

        // Optional allowlist (RZ_INSTRUMENTED_CRATES) for stricter mode.
        let allow = self.instrumented_crates_cached(tcx);
        allow.contains(crate_name)
    }

    fn place_from_operand<'tcx>(&self, op: &Operand<'tcx>) -> Option<Place<'tcx>> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => Some(*p),
            _ => None,
        }
    }

    fn compute_local_ref_use_stats<'tcx>(
        &self,
        body: &Body<'tcx>,
    ) -> HashMap<Local, LocalRefUseStats> {
        let mut stats: HashMap<Local, LocalRefUseStats> = HashMap::new();

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for stmt in &block_data.statements {
                if let StatementKind::Assign(box (place, _)) = &stmt.kind {
                    if let Some(local) = place.as_local() {
                        stats.entry(local).or_default().defs += 1;
                    }
                }
            }

            let mut counter = LocalUseCounter { stats: &mut stats };
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                counter.visit_statement(
                    stmt,
                    Location {
                        block: bb,
                        statement_index: stmt_idx,
                    },
                );
            }
            counter.visit_terminator(
                block_data.terminator(),
                Location {
                    block: bb,
                    statement_index: block_data.statements.len(),
                },
            );
        }

        stats
    }

    fn compute_summary_elidable_shared_call_ref_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> HashSet<Local> {
        if !unsafe_dataflow::use_loaded_unsafe_summaries_enabled() {
            return HashSet::new();
        }

        let local_stats = self.compute_local_ref_use_stats(body);
        let mut eligible = HashSet::new();

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            let Some(term) = &block_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                func,
                args,
                destination,
                ..
            } = &term.kind else {
                continue;
            };

            if self.is_pointer_ty(destination.ty(&body.local_decls, tcx).ty) {
                continue;
            }

            let Some((callee_did, _)) = self.direct_callee(tcx, body, block_data, func) else {
                continue;
            };
            let Some(summary) = unsafe_dataflow::summary_for_def_id(tcx, callee_did) else {
                continue;
            };

            for (arg_index, arg) in args.iter().enumerate() {
                let Some(place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                let local = place.local;
                let Some(stat) = local_stats.get(&local) else {
                    continue;
                };
                if stat.defs != 1 || stat.uses != 1 {
                    continue;
                }

                let Some(def_stmt) = block_data
                    .statements
                    .iter()
                    .find(|stmt| matches!(
                        &stmt.kind,
                        StatementKind::Assign(box (lhs, Rvalue::Ref(_, BorrowKind::Shared, src_place)))
                            if lhs.as_local() == Some(local)
                                && !src_place.projection.iter().any(|proj| matches!(proj, ProjectionElem::Deref))
                    )) else {
                    continue;
                };

                let StatementKind::Assign(box (_, Rvalue::Ref(_, BorrowKind::Shared, src_place))) =
                    &def_stmt.kind
                else {
                    continue;
                };

                let local_ty = body.local_decls[local].ty;
                if !matches!(local_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
                    continue;
                }

                if src_place
                    .projection
                    .iter()
                    .any(|proj| matches!(proj, ProjectionElem::Deref))
                {
                    continue;
                }

                let Some(arg_summary) = summary
                    .ptr_args()
                    .iter()
                    .find(|entry| entry.arg_index() == arg_index)
                else {
                    continue;
                };

                if arg_summary.reaches_direct_sink()
                    || arg_summary.escapes_to_unknown_boundary()
                    || arg_summary.forwarded_to_return()
                {
                    continue;
                }

                eligible.insert(local);
            }
        }

        eligible
    }

    fn normalize_def_path(&self, def_path: &str) -> String {
        normalize_def_path(def_path)
    }

    fn match_call_effect_rule(&self, def_path: &str) -> Option<CallEffect> {
        let def_path_norm = self.normalize_def_path(def_path);
        // Strip only a *trailing* monomorphization like `::<T>`.
        // Do NOT strip generic args that appear in the middle of a path like
        // `std::vec::Vec::<T, A>::as_mut_ptr`, otherwise we lose the method suffix.
        let def_path_no_trailing_mono = {
            let s = def_path;
            if !s.ends_with('>') {
                s
            } else if let Some(pos) = s.rfind("::<") {
                // Only treat it as a trailing monomorphization if there is no further module separator
                // after the `::<` (excluding the `::` in the `::<` itself).
                let after = &s[(pos + 3)..];
                if after.contains("::") {
                    s
                } else {
                    &s[..pos]
                }
            } else {
                s
            }
        };

        for r in CALL_EFFECT_RULES {
            let m1 = match r.kind1 {
                MatchKind::Contains => {
                    def_path.contains(r.needle1) || def_path_norm.contains(r.needle1)
                }
                MatchKind::EndsWith => {
                    def_path.ends_with(r.needle1)
                        || def_path_no_trailing_mono.ends_with(r.needle1)
                        || def_path_norm.ends_with(r.needle1)
                }
            };
            if !m1 {
                continue;
            }

            if let (Some(k2), Some(n2)) = (r.kind2, r.needle2) {
                let m2 = match k2 {
                    MatchKind::Contains => {
                        def_path.contains(n2) || def_path_norm.contains(n2)
                    }
                    MatchKind::EndsWith => {
                        def_path.ends_with(n2)
                            || def_path_no_trailing_mono.ends_with(n2)
                            || def_path_norm.ends_with(n2)
                    }
                };
                if !m2 {
                    continue;
                }
            }

            return Some(r.effect);
        }
        None
    }

    fn const_u64<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: u64) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(ConstValue::Scalar(Scalar::from_u64(v)), tcx.types.u64),
        }))
    }

    fn const_usize<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: usize) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(
                ConstValue::Scalar(Scalar::from_u64(v as u64)),
                tcx.types.usize,
            ),
        }))
    }

    fn const_u8<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: u8) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(ConstValue::Scalar(Scalar::from_u8(v)), tcx.types.u8),
        }))
    }

    fn size_operand_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if !ty.is_sized(tcx, body.typing_env(tcx)) {
            // TODO(wide-ptr): for unsized pointees (slice/str), use metadata length to compute
            // access size instead of returning 0. This would enable precise OOB checks for
            // `*const [T]` / `*const str` derefs and indexing.
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }
        // Emit MIR size_of to avoid layout normalization during instrumentation.
        SizeOperand::SizeOf(ty)
    }

    /// Compute stack-slot size for a local type.
    ///
    /// For local stack slots we want to preserve `SizeOf(ty)` even for generic ADTs where
    /// `ty.is_sized(...)` can be inconclusive during instrumentation. Those locals are still
    /// sized once monomorphized, and dropping them to size 0 loses the surrounding stack
    /// allocation metadata needed for interior references.
    fn size_operand_for_stack_local_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        match ty.kind() {
            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..) | TyKind::Foreign(..) => {
                SizeOperand::Const(self.const_usize(tcx, span, 0))
            }
            _ => self.size_operand_for_ty(tcx, body, ty, span),
        }
    }

    /// Compute access size for a deref of `ptr_local` producing `access_ty`.
    /// For wide pointers to slices/str, derive the size from pointer metadata.
    fn size_operand_for_deref<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        access_ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if access_ty.is_sized(tcx, body.typing_env(tcx)) {
            return self.size_operand_for_ty(tcx, body, access_ty, span);
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return SizeOperand::Const(self.const_usize(tcx, span, 0)),
        };

        match pointee.kind() {
            TyKind::Slice(elem_ty) => SizeOperand::PtrMetadataSlice {
                ptr_local,
                elem_ty: *elem_ty,
            },
            TyKind::Str => SizeOperand::PtrMetadataStr { ptr_local },
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    /// Best-effort bounds length for wide pointers (slice/str), in bytes.
    /// Returns 0 for thin pointers or unknown metadata.
    fn bounds_len_operand_for_ptr_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return SizeOperand::Const(self.const_usize(tcx, span, 0)),
        };

        match pointee.kind() {
            TyKind::Slice(elem_ty) => SizeOperand::PtrMetadataSlice {
                ptr_local,
                elem_ty: *elem_ty,
            },
            TyKind::Str => SizeOperand::PtrMetadataStr { ptr_local },
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    /// Bounds length for direct reference creation.
    ///
    /// Unlike raw derives, an `Rvalue::Ref` still names the concrete pointee object, so for sized
    /// pointees we can safely use `size_of::<T>()` as the dynamic bounds. This preserves array /
    /// aggregate extents across later unsizing or vectorized loads (e.g. `&[u8; 64]` feeding SIMD
    /// lane reads) without constraining subsequent raw-pointer arithmetic.
    fn ref_creation_bounds_len_operand_for_ptr_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) => *pointee,
            _ => return self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, span),
        };

        match pointee.kind() {
            TyKind::Slice(elem_ty) => SizeOperand::PtrMetadataSlice {
                ptr_local,
                elem_ty: *elem_ty,
            },
            TyKind::Str => SizeOperand::PtrMetadataStr { ptr_local },
            _ if pointee.is_sized(tcx, body.typing_env(tcx)) => {
                self.size_operand_for_ty(tcx, body, pointee, span)
            }
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    fn ptr_ty_has_precise_wide_bounds<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str)
            }
            _ => false,
        }
    }

    fn should_forward_bounds_from_src_ptr_derive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Local,
        dst: Local,
    ) -> bool {
        let src_ty = body.local_decls[src].ty;
        let dst_ty = body.local_decls[dst].ty;
        self.ptr_ty_has_precise_wide_bounds(src_ty) && self.is_thin_ptr_ty(tcx, body, dst_ty)
    }

    fn field_offset_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_ty: Ty<'tcx>,
        variant: Option<VariantIdx>,
        field: FieldIdx,
    ) -> Option<u64> {
        if base_ty.has_param()
            || base_ty.has_infer()
            || base_ty.has_aliases()
            || base_ty.has_opaque_types()
            || base_ty.has_placeholders()
        {
            return None;
        }

        let typing_env = body.typing_env(tcx);
        let input = PseudoCanonicalInput { typing_env, value: base_ty };
        let layout = tcx.layout_of(input).ok()?;
        let cx = rustc_middle::ty::layout::LayoutCx::new(tcx, typing_env);
        let layout = if let Some(variant_idx) = variant {
            layout.for_variant(&cx, variant_idx)
        } else {
            layout
        };

        Some(layout.fields.offset(field.index()).bytes())
    }

    fn cast_index_to_usize<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        idx_op: Operand<'tcx>,
        idx_ty: Ty<'tcx>,
    ) -> (Operand<'tcx>, Vec<Statement<'tcx>>) {
        if idx_ty == tcx.types.usize {
            return (idx_op, Vec::new());
        }
        let idx_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.usize, source_info.span));
        let stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(idx_local),
                Rvalue::Cast(CastKind::IntToInt, idx_op, tcx.types.usize),
            ))),
        );
        (Operand::Copy(Place::from(idx_local)), vec![stmt])
    }

    fn offset_stmts_for_projection<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        base_ptr_local: Local,
        projection: &[PlaceElem<'tcx>],
        addr_local: Local,
    ) -> Option<Vec<Statement<'tcx>>> {
        if projection.is_empty() {
            return Some(Vec::new());
        }

        let base_ptr_ty = body.local_decls[base_ptr_local].ty;
        let base_pointee = base_ptr_ty.builtin_deref(true)?;
        let mut place_ty = PlaceTy::from_ty(base_pointee);

        let mut offset_local: Option<Local> = None;
        let mut stmts: Vec<Statement<'tcx>> = Vec::new();

        let mut ensure_offset_local = |body: &mut Body<'tcx>,
                                       stmts: &mut Vec<Statement<'tcx>>|
         -> Local {
            if let Some(l) = offset_local {
                return l;
            }
            let l = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));
            let init_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(l),
                    Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                ))),
            );
            stmts.push(init_stmt);
            offset_local = Some(l);
            l
        };

        for proj in projection.iter() {
            match proj {
                ProjectionElem::Deref => {
                    // Nested deref: we do not try to follow multiple levels here.
                    return None;
                }
                ProjectionElem::Downcast(_, _variant_idx) => {
                    // Keep `place_ty` unchanged here and let the canonical
                    // `projection_ty` update happen once at the end of the loop.
                    // Setting `variant_index` manually here and then calling
                    // `projection_ty(Downcast)` again triggers rustc's
                    // "non field projection on downcasted place" ICE.
                }
                ProjectionElem::Field(field, _) => {
                    let offset_bytes =
                        self.field_offset_bytes(tcx, body, place_ty.ty, place_ty.variant_index, *field)?;
                    if offset_bytes != 0 {
                        let off_local = ensure_offset_local(body, &mut stmts);
                        let add_stmt = Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(off_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(off_local)),
                                        self.const_usize(tcx, source_info.span, offset_bytes as usize),
                                    )),
                                ),
                            ))),
                        );
                        stmts.push(add_stmt);
                    }
                }
                ProjectionElem::Index(idx_local) => {
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_ty = body.local_decls[*idx_local].ty;
                    // `ProjectionElem::Index` is expected to use an integer local, but in
                    // complex optimized MIR we occasionally see non-scalar locals here.
                    // Bail out to the base-address fallback rather than emitting invalid MIR.
                    if !idx_ty.is_integral() {
                        return None;
                    }
                    let idx_op = Operand::Copy(Place::from(*idx_local));
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, idx_ty);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        &size_op,
                    );
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(
                                BinOp::Mul,
                                Box::new((idx_usize_op, elem_size_op)),
                            ),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                ProjectionElem::ConstantIndex { offset, from_end, .. } => {
                    if *from_end {
                        return None;
                    }
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_op = self.const_usize(tcx, source_info.span, *offset as usize);
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, tcx.types.usize);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        &size_op,
                    );
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(
                                BinOp::Mul,
                                Box::new((idx_usize_op, elem_size_op)),
                            ),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                ProjectionElem::Subslice { from, from_end, .. } => {
                    if *from_end {
                        return None;
                    }
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_op = self.const_usize(tcx, source_info.span, *from as usize);
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, tcx.types.usize);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        &size_op,
                    );
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(
                                BinOp::Mul,
                                Box::new((idx_usize_op, elem_size_op)),
                            ),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                _ => return None,
            }

            place_ty = place_ty.projection_ty(tcx, *proj);
        }

        if let Some(off_local) = offset_local {
            let add_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(addr_local),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Box::new((
                            Operand::Copy(Place::from(addr_local)),
                            Operand::Copy(Place::from(off_local)),
                        )),
                    ),
                ))),
            );
            stmts.push(add_stmt);
        }

        Some(stmts)
    }

    fn addr_stmts_for_access_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
    ) -> Option<(Option<Statement<'tcx>>, Statement<'tcx>, Vec<Statement<'tcx>>)> {
        let has_deref = place
            .projection
            .iter()
            .next()
            .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
        if has_deref {
            let base_place = Place::from(place.local);
            let (opt, stmt) = self.addr_stmts_for_place(tcx, body, source_info, base_place, addr_local)?;
            let offset_stmts =
                self.offset_stmts_for_projection(tcx, body, source_info, place.local, &place.projection[1..], addr_local)?;
            return Some((opt, stmt, offset_stmts));
        }

        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_pointer_ty(place_ty) {
            let (opt, stmt) = self.addr_stmts_for_place(tcx, body, source_info, place, addr_local)?;
            return Some((opt, stmt, Vec::new()));
        }

        None
    }

    /// Returns true when alias checks should be skipped for this pointee type.
    /// We conservatively opt out if the type may contain UnsafeCell or if it is
    /// not fully known/normalizable in the current typing context.
    fn alias_exempt_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        // SB-lite currently approximates Rust's aliasing rules using a per-allocation
        // borrow stack, but it does **not** model interior mutability soundly.
        //
        // In Rust, types that contain an `UnsafeCell` are *not* `Freeze`, meaning they
        // may be legally mutated through a shared reference (via `Cell`/`RefCell` or
        // other unsafe code patterns). Treating such accesses as ordinary shared reads
        // and enforcing "no write while shared is live" would yield many false positives
        // in real-world code (e.g., `bytes::BytesMut` internals).
        //
        // Policy: if a pointee type is not `Freeze` (or we cannot reliably reason about
        // it in this typing context), mark derived tags as `alias_exempt` so the runtime
        // skips SB-lite enforcement for that tag. This is a deliberate precision/soundness
        // trade-off: missing metadata is acceptable; incorrect metadata is not.
        // Keep alias checks enabled for slice/str pointees even in generic code:
        // `from_raw_parts_mut`-style wrappers are often generic and would otherwise
        // be fully exempt, hiding core aliasing violations.
        if matches!(ty.kind(), TyKind::Slice(_) | TyKind::Str) {
            return false;
        }

        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return true;
        }
        let typing_env = body.typing_env(tcx);
        !ty.is_freeze(tcx, typing_env)
    }

    fn alias_exempt_for_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                self.alias_exempt_for_ty(tcx, body, *pointee)
            }
            _ => false,
        }
    }

    fn layout_size_bytes<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> usize {
        // `tcx.layout_of(...)` can trigger normalization and will hard-error (E0080)
        // for types that are not fully normalizable in the current context, e.g.
        // `&[<I as Iterator>::Item; 0]` inside generic code like `Splice<'_, I, N>::drop`.
        //
        // For our instrumentation, "unknown size" is fine: we already treat size=0 as
        // best-effort and avoid precise OOB checks in that case.
        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return 0;
        }

        let input = PseudoCanonicalInput {
            typing_env: TypingEnv::fully_monomorphized(),
            value: ty,
        };
        tcx.layout_of(input)
            .ok()
            .map(|l| l.size.bytes() as usize)
            .unwrap_or(0)
    }

    fn type_needs_normalization<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        struct NeedsNormalizationVisitor;

        impl<'tcx> TypeVisitor<TyCtxt<'tcx>> for NeedsNormalizationVisitor {
            type Result = ControlFlow<()>;

            fn visit_ty(&mut self, ty: Ty<'tcx>) -> Self::Result {
                match ty.kind() {
                    TyKind::Alias(..)
                    | TyKind::Param(..)
                    | TyKind::Bound(..)
                    | TyKind::Placeholder(..)
                    | TyKind::Infer(..)
                    | TyKind::Error(..) => ControlFlow::Break(()),
                    _ => ty.super_visit_with(self),
                }
            }

            fn visit_const(&mut self, c: rustc_middle::ty::Const<'tcx>) -> Self::Result {
                match c.kind() {
                    TyConstKind::Param(..)
                    | TyConstKind::Infer(..)
                    | TyConstKind::Bound(..)
                    | TyConstKind::Placeholder(..)
                    | TyConstKind::Unevaluated(..)
                    | TyConstKind::Expr(..)
                    | TyConstKind::Error(..) => ControlFlow::Break(()),
                    _ => c.super_visit_with(self),
                }
            }
        }

        let mut v = NeedsNormalizationVisitor;
        ty.visit_with(&mut v).is_break()
    }

    // Resolve const/promoted pointers to their global allocation metadata (size + offset).
    fn const_alloc_info<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        c: &ConstOperand<'tcx>,
    ) -> Option<ConstAllocInfo> {
        let const_ty = c.const_.ty();
        if const_ty.has_param()
            || const_ty.has_infer()
            || const_ty.has_aliases()
            || const_ty.has_opaque_types()
            || const_ty.has_placeholders()
            || const_ty.has_bound_vars()
            || self.type_needs_normalization(const_ty)
        {
            return None;
        }

        let scalar = c
            .const_
            .try_eval_scalar(tcx, TypingEnv::fully_monomorphized())?;
        let ptr = scalar.to_pointer(&tcx).discard_err()?;
        let (prov_opt, offset) = ptr.into_raw_parts();
        let prov = prov_opt?;
        let alloc_id = prov.alloc_id();

        let size = match tcx.global_alloc(alloc_id) {
            GlobalAlloc::Memory(mem) => mem.inner().size().bytes() as usize,
            GlobalAlloc::Static(def_id) => {
                let ty = tcx.type_of(def_id).skip_binder();
                // Same issue as above: statics can have types that still require
                // normalization/projection evaluation in ways that can ICE/error.
                // Unknown is fine.
                if ty.has_param()
                    || ty.has_infer()
                    || ty.has_aliases()
                    || ty.has_opaque_types()
                    || ty.has_placeholders()
                {
                    0
                } else {
                    self.layout_size_bytes(tcx, ty)
                }
            }
            _ => return None,
        };

        Some(ConstAllocInfo {
            size,
            base_offset: offset.bytes() as usize,
        })
    }

    fn const_fn_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        c: &ConstOperand<'tcx>,
    ) -> Option<DefId> {
        let mut def_id_opt: Option<DefId> = None;

        let const_ty = c.const_.ty();
        if let TyKind::FnDef(def_id, args) = const_ty.kind() {
            return Some(self.resolve_instance_def_id(tcx, body, *def_id, args));
        }
        if const_ty.has_param()
            || const_ty.has_infer()
      
            || const_ty.has_aliases()
            || const_ty.has_opaque_types()
            || const_ty.has_placeholders()
            || const_ty.has_bound_vars()
            || self.type_needs_normalization(const_ty)
        {
            return None;
        }

        if let Some(scalar) =
            c.const_.try_eval_scalar(tcx, TypingEnv::fully_monomorphized())
        {
            if let Some(ptr) = scalar.to_pointer(&tcx).discard_err() {
                let (prov_opt, _offset) = ptr.into_raw_parts();
                if let Some(prov) = prov_opt {
                    let alloc_id = prov.alloc_id();
                    if let GlobalAlloc::Function { instance } = tcx.global_alloc(alloc_id) {
                        def_id_opt = Some(self.resolve_instance_def_id(
                            tcx,
                            body,
                            instance.func_def_id(),
                            instance.args,
                        ));
                    }
                }
            }
        }

        def_id_opt
    }

    fn resolve_instance_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        def_id: DefId,
        args: GenericArgsRef<'tcx>,
    ) -> DefId {
        // Call-boundary tag ids must use the concrete impl instance, not the trait method item.
        // If we keep the trait item DefId here, push and take end up keyed differently.
        let typing_env = body.typing_env(tcx);
        let normalized_args = tcx.try_normalize_erasing_regions(typing_env, args).unwrap_or(args);
        Instance::try_resolve(tcx, typing_env, def_id, normalized_args)
            .ok()
            .flatten()
            .map(|instance| instance.def_id())
            .unwrap_or(def_id)
    }

    /// If `fat_local` is a fat pointer local (e.g. `&[T]`), try to find a thin "base" pointer local
    /// it was coerced from via `PointerCoercion(Unsize, ...)` in the *same basic block*.
    ///
    /// This is a best-effort workaround to propagate tags through patterns like:
    ///   _3 = move _4 as &[i32] (PointerCoercion(Unsize, Implicit));
    ///   _2 = core::slice::<impl [i32]>::as_ptr(move _3);
    fn backtrack_unsize_base_local<'tcx>(
        &self,
        fat_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        for stmt in statements.iter().rev() {
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
            if place.as_local() != Some(fat_local) {
                continue;
            }

            if let Rvalue::Cast(CastKind::PointerCoercion(_, _), op, _) = rvalue {
                if let Some(src_place) = self.place_from_operand(op) {
                    return Some(src_place.local);
                }
            }

            // Stop once we found the most recent definition of `fat_local`, even if it wasn't an unsize cast.
            return None;
        }
        None
    }

    /// Backtrack a tuple/aggregate assignment in the same block to find the source local
    /// for a projected field, if that operand is a pointer local.
    fn backtrack_aggregate_field_local<'tcx>(
        &self,
        agg_local: Local,
        field_idx: usize,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        for stmt in statements.iter().rev() {
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
            if place.as_local() != Some(agg_local) {
                continue;
            }

            if let Rvalue::Aggregate(_kind, ops) = rvalue {
                if let Some(op) = ops.iter().nth(field_idx) {
                    if let Some(p) = self.place_from_operand(op) {
                        return Some(p.local);
                    }
                }
            }

            // Stop once we found the most recent definition of `agg_local`.
            return None;
        }
        None
    }

    /// Backtrack an aggregate field globally when `agg_local` has exactly one aggregate
    /// definition in the function body.
    ///
    /// This is a conservative recovery for enum/aggregate wrappers such as
    /// `Result<&T, E>` or `ControlFlow<_, &T>` where the pointer is stored in a non-pointer
    /// local and later extracted through a `Downcast + Field` projection in a different block.
    fn backtrack_global_aggregate_field_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        agg_local: Local,
        field_idx: usize,
    ) -> Option<Local> {
        let mut recovered: Option<Option<Local>> = None;

        for block_data in body.basic_blocks.iter() {
            for stmt in &block_data.statements {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
                if place.as_local() != Some(agg_local) {
                    continue;
                }

                let local = match rvalue {
                    Rvalue::Aggregate(_kind, ops) => ops
                        .iter()
                        .nth(field_idx)
                        .and_then(|op| self.place_from_operand(op))
                        .map(|p| p.local),
                    _ => return None,
                };

                match recovered {
                    Some(existing) if existing != local => return None,
                    Some(_) => {}
                    None => recovered = Some(local),
                }
            }
        }

        recovered.flatten()
    }

    fn downcast_field_projection_index<'tcx>(&self, place: Place<'tcx>) -> Option<usize> {
        let (last, prefix) = place.projection.split_last()?;
        let ProjectionElem::Field(field, _) = last else {
            return None;
        };
        if prefix
            .iter()
            .all(|pe| matches!(pe, ProjectionElem::Downcast(..)))
        {
            Some(field.index())
        } else {
            None
        }
    }

    /// Resolve a pointer local backing a call argument place.
    ///
    /// For plain pointer locals, return the local directly.
    /// For one-step field projections like `_agg.0`, backtrack the aggregate assignment
    /// in the same block and return the source local used for that field when pointer-typed.
    fn resolve_ptr_local_for_call_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        place: Place<'tcx>,
    ) -> Option<Local> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(place_ty) {
            return None;
        }

        if place.projection.is_empty() {
            let local = place.local;
            if self.is_pointer_ty(body.local_decls[local].ty) {
                return Some(local);
            }
            return None;
        }

        if place.projection.len() == 1 {
            if let ProjectionElem::Field(field, _ty) = place.projection[0] {
                if let Some(src_local) = self.backtrack_aggregate_field_local(
                    place.local,
                    field.index(),
                    &block_data.statements,
                ) {
                    if self.is_pointer_ty(body.local_decls[src_local].ty) {
                        return Some(src_local);
                    }
                }
            }
        }

        None
    }

    /// Backtrack a deref'ed pointer local to the base local it was borrowed from, if any.
    ///
    /// Goal: recover the *stack* local that actually owns storage when MIR takes an address
    /// through a deref projection, e.g. `&raw const (*_r)` or `&_r` where `_r: &T`.
    ///
    /// Constraints and policy:
    /// - Same-block only, walking backwards from `ptr_local`'s last definition.
    /// - Follow only ref/rawptr creations whose source place is **not** a deref projection.
    ///   This keeps us from treating heap/foreign pointees as stack locals.
    /// - Allow simple pointer-local forwarding (`Use`, `CopyForDeref`, and pointer casts)
    ///   to find the original ref/rawptr creation.
    /// - Return `None` on ambiguity or if we would cross a deref boundary.
    ///
    /// This is intentionally conservative: missing metadata is acceptable; incorrect metadata is not.
    fn backtrack_deref_base_local<'tcx>(
        &self,
        ptr_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        let mut current_local = ptr_local;
        let mut search_end = statements.len();

        'outer: loop {
            for (idx, stmt) in statements[..search_end].iter().enumerate().rev() {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
                if place.as_local() != Some(current_local) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(_, _, src_place) | Rvalue::RawPtr(_, src_place) => {
                        let is_deref_src = src_place
                            .projection
                            .iter()
                            .next()
                            .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                        if is_deref_src {
                            return None;
                        }
                        return Some(src_place.local);
                    }
                    Rvalue::Use(op) => {
                        let Some(next_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    Rvalue::CopyForDeref(p) => {
                        let Some(next_local) = p.as_local() else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _to_ty,
                    )
                    | Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _to_ty) => {
                        let Some(next_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    _ => return None,
                }
            }

            return None;
        }
    }

    /// Recover the pointee local for a mutable-reference local in the same block.
    ///
    /// Typical shape:
    ///   _tmp = &mut _p;
    ///   call(..., copy _tmp, ...);
    ///
    /// Returns `_p` for `_tmp`. We also follow trivial local forwarding
    /// (`Use`, `CopyForDeref`, and pointer casts) to tolerate MIR temporaries.
    fn backtrack_mut_ref_pointee_local<'tcx>(
        &self,
        ref_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        let mut current_local = ref_local;
        let mut search_end = statements.len();

        'outer: loop {
            for (idx, stmt) in statements[..search_end].iter().enumerate().rev() {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
                if place.as_local() != Some(current_local) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(_, BorrowKind::Mut { .. }, src_place) => {
                        return Some(src_place.local);
                    }
                    Rvalue::Use(op) => {
                        let Some(next_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    Rvalue::CopyForDeref(p) => {
                        let Some(next_local) = p.as_local() else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _,
                    )
                    | Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                        let Some(next_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        else {
                            return None;
                        };
                        current_local = next_local;
                        search_end = idx;
                        continue 'outer;
                    }
                    _ => return None,
                }
            }

            return None;
        }
    }

    /// Best-effort: recover a pointer-typed source local that feeds `dst_local` in the same block.
    ///
    /// This follows trivial forwarding/casts and ref/raw creations:
    /// - `Use`, `CopyForDeref`
    /// - pointer casts/coercions/transmute
    /// - `Ref` / `RawPtr` assignments (returns their source local when pointer-typed)
    ///
    /// We use it to avoid falling back to `parent=0` for ref/raw creations when the immediate
    /// source local is a temporary projection local instead of the real pointer carrier.
    fn backtrack_pointer_source_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        dst_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        let mut current_local = dst_local;
        let mut search_end = statements.len();

        'outer: loop {
            for (idx, stmt) in statements[..search_end].iter().enumerate().rev() {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else { continue };
                if place.as_local() != Some(current_local) {
                    continue;
                }

                let next_local = match rvalue {
                    Rvalue::Use(op) => self.place_from_operand(op).and_then(|p| {
                        p.as_local().or_else(|| {
                            if self.is_pointer_ty(body.local_decls[p.local].ty) {
                                Some(p.local)
                            } else {
                                None
                            }
                        })
                    }),
                    Rvalue::CopyForDeref(p) => p.as_local(),
                    Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _,
                    )
                    | Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                        self.place_from_operand(op).and_then(|p| {
                            p.as_local().or_else(|| {
                                if self.is_pointer_ty(body.local_decls[p.local].ty) {
                                    Some(p.local)
                                } else {
                                    None
                                }
                            })
                        })
                    }
                    Rvalue::Ref(_, _, src_place) | Rvalue::RawPtr(_, src_place) => {
                        Some(src_place.local)
                    }
                    _ => return None,
                };

                let Some(next_local) = next_local else { return None };

                if self.is_pointer_ty(body.local_decls[next_local].ty) {
                    return Some(next_local);
                }

                current_local = next_local;
                search_end = idx;
                continue 'outer;
            }

            return None;
        }
    }

    /// Resolve the best parent-tag operand for ref/raw creation from `src_place`.
    ///
    /// We first try the nearest ref-ancestor tag local, then the normal pointer tag local.
    /// If `src_place.local` is not pointer-typed, we backtrack same-block assignments to find
    /// the pointer carrier that produced it.
    ///
    /// Example (base64 encode loop style):
    /// Rust:
    ///   let chunk = &mut out[out_idx..out_idx + 4];
    ///   chunk[0] = ...
    ///
    /// MIR-like shape:
    ///   _tmp = &mut (*_out_slice)[_idx.._idx+4];
    ///   _elt = &mut (*_tmp)[0];
    ///
    /// The immediate `src_place.local` for `_elt` can be a projection-heavy temp with no tag local.
    /// If we use only that local, parent becomes `0` and the new ref is treated as a root sibling.
    /// Backtracking recovers `_out_slice` (or another pointer carrier), so parent lineage is kept.
    fn recover_parent_source_local_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
    ) -> Option<Local> {
        let mut candidate_local: Option<Local> = None;
        let block_stmts = &body.basic_blocks[bb].statements;
        let upto = stmt_idx.min(block_stmts.len());

        let src_local = src_place.local;
        if self.is_pointer_ty(body.local_decls[src_local].ty) {
            // For projected sources like `(*tmp)[i]`, `(*tmp)[a..b]`, or `(*tmp).field`,
            // `src_local` is often a short-lived wrapper temp created by optimized MIR. Its tag
            // can be a root-like raw helper instead of the real parent lineage we want the new
            // ref/raw to inherit from. Prefer backtracking through simple same-block forwarding
            // first, and only fall back to the immediate local if that fails.
            let has_complex_projection = !src_place.projection.is_empty()
                && !(src_place.projection.len() == 1
                    && matches!(src_place.projection[0], ProjectionElem::Deref));
            if has_complex_projection {
                candidate_local =
                    self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto]);
                if candidate_local.is_some() {
                    // Keep the recovered source instead of the projection temp.
                } else {
                    candidate_local = Some(src_local);
                }
            } else {
            // For raw creation from projected pointer-field loads (`(*ref_to_struct).ptr_field`),
            // using `src_place.local` as parent incorrectly picks the container-ref tag.
            // That ties the raw pointer to the stack slot of the wrapper object instead of the
            // real pointee carried in the field.
            let projected_raw_field_load = is_raw_creation
                && !src_place.projection.is_empty()
                && matches!(
                    src_place.projection.first(),
                    Some(ProjectionElem::Deref)
                )
                && src_place
                    .projection
                    .iter()
                    .skip(1)
                    .any(|pe| matches!(pe, ProjectionElem::Field(_, _)));
            if projected_raw_field_load {
                candidate_local =
                    self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto]);
            } else {
                candidate_local = Some(src_local);
            }
            }
        } else {
            candidate_local = self.backtrack_pointer_source_local(
                body,
                src_local,
                &block_stmts[..upto],
            );
        }

        if candidate_local.is_none() && !src_place.projection.is_empty() {
            // Field/subslice-heavy places like `self.buffer[a..b]` often have a non-pointer carrier
            // local (`Vec<T>`, struct field, tuple field) even though an earlier projection prefix
            // is pointer-typed (`self`, `&mut self.field`, etc.). Using the nearest pointer-typed
            // prefix local preserves the surrounding borrow family instead of dropping straight to
            // a raw root for the projected child.
            for prefix_len in (0..src_place.projection.len()).rev() {
                let prefix = PlaceRef {
                    local: src_place.local,
                    projection: &src_place.projection[..prefix_len],
                }
                .to_place(tcx);
                let prefix_ty = prefix.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(prefix_ty) {
                    candidate_local = Some(prefix.local);
                    break;
                }
            }
        }

        candidate_local
    }

    fn recover_pointer_source_local_for_projected_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
    ) -> Option<Local> {
        let block_data = &body.basic_blocks[bb];
        let mut src_local_opt = self.recover_parent_source_local_for_place(
            tcx,
            body,
            bb,
            stmt_idx,
            src_place,
            is_raw_creation,
        );

        if src_local_opt.is_none() && !self.is_pointer_ty(body.local_decls[src_place.local].ty) {
            src_local_opt = self
                .backtrack_single_pointer_arg_call_result_source_local(
                    tcx,
                    body,
                    bb,
                    src_place.local,
                );
        }

        if src_local_opt.is_none()
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place
                .projection
                .iter()
                .skip(1)
                .any(|pe| matches!(pe, ProjectionElem::Field(_, _)))
            && self.is_pointer_ty(body.local_decls[src_place.local].ty)
        {
            src_local_opt = Some(src_place.local);
        }

        if src_local_opt.is_none()
            && !src_place.projection.is_empty()
            && matches!(src_place.projection[0], ProjectionElem::Deref)
        {
            if let Some(backtracked_local) = self.backtrack_deref_base_local(
                src_place.local,
                &block_data.statements[..stmt_idx],
            ) {
                if self.is_pointer_ty(body.local_decls[backtracked_local].ty) {
                    src_local_opt = Some(backtracked_local);
                }
            }
        }

        if !src_place.projection.is_empty() {
            let base_local = src_place.local;
            let base_ty = body.local_decls[base_local].ty;
            if self.is_pointer_ty(base_ty) && !self.is_thin_ptr_ty(tcx, body, base_ty) {
                if src_place.projection.len() >= 1
                    && matches!(src_place.projection[0], ProjectionElem::Deref)
                {
                    if src_place.projection.len() >= 2 {
                        if let ProjectionElem::Field(field, _) = src_place.projection[1] {
                            if field.index() == 0 {
                                src_local_opt = Some(base_local);
                            }
                        }
                    }
                } else if let ProjectionElem::Field(field, _) = src_place.projection[0] {
                    if field.index() == 0 {
                        src_local_opt = Some(base_local);
                    }
                }
            }
        }

        if src_local_opt.is_none() {
            if let Some(field_idx) = self.downcast_field_projection_index(src_place) {
                src_local_opt = self.backtrack_aggregate_field_local(
                    src_place.local,
                    field_idx,
                    &block_data.statements[..stmt_idx],
                );
                if src_local_opt.is_none() {
                    src_local_opt = self.backtrack_global_aggregate_field_local(
                        body,
                        src_place.local,
                        field_idx,
                    );
                }
            }
        }

        src_local_opt
    }

    fn parent_tag_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        src_place: Place<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        is_raw_creation: bool,
    ) -> Operand<'tcx> {
        let candidate_local = if src_place.projection.is_empty() {
            let block_stmts = &body.basic_blocks[bb].statements;
            let upto = stmt_idx.min(block_stmts.len());
            let src_local = src_place.local;
            if self.is_pointer_ty(body.local_decls[src_local].ty) {
                let projected_raw_field_load = is_raw_creation
                    && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
                    && src_place
                        .projection
                        .iter()
                        .skip(1)
                        .any(|pe| matches!(pe, ProjectionElem::Field(_, _)));
                if projected_raw_field_load {
                    self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto])
                } else {
                    Some(src_local)
                }
            } else {
                self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto])
            }
        } else {
            self.recover_pointer_source_local_for_projected_place(
                tcx,
                body,
                bb,
                stmt_idx,
                src_place,
                is_raw_creation,
            )
        };

        if let Some(local) = candidate_local {
            // Prefer the source local's concrete tag first.
            // The ref-ancestor slot may hold a synthetic zero in valid flows
            // (e.g., after RawRoot), and using it first drops provenance.
            if let Some(tl) = tag_local_for_ptr_local.get(&local) {
                return Operand::Copy(Place::from(*tl));
            }
            if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&local) {
                return Operand::Copy(Place::from(*tl));
            }
        }

        self.const_u64(tcx, source_info.span, 0)
    }

    /// Compute the set of stack locals worth tracking as allocations.
    ///
    /// We track locals whose address is taken (via `&` / `&raw`) so range-based allocation
    /// lookup and OOB checks work for stack data. This includes "address-of through deref"
    /// patterns that appear in optimized MIR, such as:
    ///   _r = &_x;
    ///   _p = &raw const (*_r);
    /// In that case, we must treat `_x` as address-taken (not just `_r`), otherwise no
    /// StackAlloc is emitted for the actual stack slot and the runtime will see WILD_POINTER.
    ///
    /// Pointer-typed locals are included only when *their own* address is taken (e.g., `&&T`),
    /// which avoids false OOB reports when simply reading a pointer value through `*const *const T`.
    fn compute_interesting_stack_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> HashSet<Local> {
        let mut interesting: HashSet<Local> = HashSet::new();
        for (_bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                if let StatementKind::Assign(box (_dst, rv)) = &stmt.kind {
                    match rv {
                        // Address-taken locals: these correspond to real stack slots that pointers can reference.
                        Rvalue::Ref(_, _bk, src_place) => {
                            interesting.insert(src_place.local);
                            let is_deref_src = src_place
                                .projection
                                .iter()
                                .next()
                                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                            if is_deref_src {
                                if let Some(base_local) = self.backtrack_deref_base_local(
                                    src_place.local,
                                    &block_data.statements[..stmt_idx],
                                ) {
                                    if base_local != RETURN_PLACE {
                                        interesting.insert(base_local);
                                    }
                                }
                            }
                        }
                        Rvalue::RawPtr(_mutbl, src_place) => {
                            interesting.insert(src_place.local);
                            let is_deref_src = src_place
                                .projection
                                .iter()
                                .next()
                                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                            if is_deref_src {
                                if let Some(base_local) = self.backtrack_deref_base_local(
                                    src_place.local,
                                    &block_data.statements[..stmt_idx],
                                ) {
                                    if base_local != RETURN_PLACE {
                                        interesting.insert(base_local);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(term) = &block_data.terminator {
                match &term.kind {
                    TerminatorKind::Drop { place, .. } => {
                        // Drop glue implicitly takes `&place` even if MIR has no explicit ref/raw.
                        // Treat Drop as an implicit address-of so stack slots are tracked.
                        let local = place.local;
                        interesting.insert(local);
                    }
                    _ => {}
                }
            }
        }
        interesting
    }

    fn track_all_stack_allocs_flag(&self) -> bool {
        false
    }

    fn entry_insert_after_prologue<'tcx>(&self, body: &Body<'tcx>) -> usize {
        let entry_bd = &body.basic_blocks[START_BLOCK];
        let mut idx = 0usize;
        while idx < entry_bd.statements.len() {
            match entry_bd.statements[idx].kind {
                StatementKind::StorageLive(_) => idx += 1,
                _ => break,
            }
        }
        idx
    }

    fn push_arg_retags_at_entry<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        entry_stmt_idx: usize,
    ) {
        let entry_bb = START_BLOCK;
        let entry_source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };
        let callee_id = self.callee_id_u64(tcx, body.source.def_id());

        // Retag pointer arguments. Wide pointers are handled by extracting the data pointer
        // during lowering, so we can safely retag them here.
        for (arg_index, arg_local) in body.args_iter().enumerate() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_pointer_ty(arg_ty) {
                ptr_locals_needing_tag.insert(arg_local);
                tagged_ptr_locals.insert(arg_local);
                insert_points.push(InsertPoint {
                    bb: entry_bb,
                    stmt_idx: entry_stmt_idx,
                    insert_before: false,
                    source_info: entry_source_info,
                    place: Place::from(arg_local),
                    kind: InstrKind::ArgRetag {
                        callee_id,
                        arg_index: arg_index as u64,
                        ptr_local: arg_local,
                    },
                });
            }
        }
    }

    fn scan_statement<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        stmt_idx: usize,
        stmt: &Statement<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
        summary_elidable_shared_call_ref_locals: &HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
    ) {
        // Stack allocation lifetime: StorageLive/StorageDead.
        match stmt.kind {
            StatementKind::StorageDead(local) => {
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // NOTE: optimized MIR can place StorageDead before the last use
                    // through an outstanding reference. Emitting dead-by-default here
                    // causes false UAFs, so this remains opt-in.
                    if !self.use_storage_dead_enabled() {
                        return;
                    }
                    let ty = body.local_decls[local].ty;
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op =
                            self.size_operand_for_stack_local_ty(tcx, body, ty, stmt.source_info.span);
                        if !matches!(size_op, SizeOperand::Const(_)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: false,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                false,
                                &size_op,
                                "StorageDead",
                            );
                        }
                    }
                    return;
                }
            }
            StatementKind::StorageLive(local) => {
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    let ty = body.local_decls[local].ty;

                    // Record pointer-typed locals only if their address is taken (interesting locals).
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op =
                            self.size_operand_for_stack_local_ty(tcx, body, ty, stmt.source_info.span);
                        if !matches!(size_op, SizeOperand::Const(_)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: true,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                true,
                                &size_op,
                                "StorageLive",
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        // Pointer read: plain deref load in a statement, e.g. `_dst = copy (*p)` or `_dst = move (*p)`.
        // This is not a call/intrinsic, so we must classify it explicitly as a READ.
        if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
            if let Rvalue::Use(op) = rhs {
                let deref_place: Option<&Place<'tcx>> = match op {
                    Operand::Copy(p) | Operand::Move(p) => Some(p),
                    _ => None,
                };

                if let Some(p) = deref_place {
                    let is_deref_read = p
                        .projection
                        .iter()
                        .next()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if is_deref_read {
                        // `p` is a deref place `(*ptr_local) ...` so the base pointer local is `p.local`.
                        let ptr_local = p.local;
                        let ptr_ty = body.local_decls[ptr_local].ty;
                        // Skip reads through &'static references. These point to global memory
                        // that we don't track, and treating them as wild would be a false positive.
                        let skip_static_ref_read =
                            matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                        let skip_vtable_read = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                        if !skip_static_ref_read && !skip_vtable_read && self.is_pointer_ty(ptr_ty) {
                            // Best-effort size: use the type of the dereferenced/read place.
                            let loaded_ty = p.ty(&body.local_decls, tcx).ty;
                            let read_ty = loaded_ty;
                            // Loading function pointers or vtable-like structs should not trigger
                            // memory access checks; treat these as benign metadata reads.
                            let skip_fn_ptr_read = matches!(
                                loaded_ty.kind(),
                                TyKind::FnPtr(..) | TyKind::FnDef(..)
                            ) || self.is_fn_table_adt_ty(tcx, loaded_ty);
                            let skip_vtable_field_read = match read_ty.kind() {
                                TyKind::FnPtr(..) | TyKind::FnDef(..) => true,
                                TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                                    self.is_fn_table_adt_ty(tcx, *pointee)
                                }
                                _ => self.is_fn_table_adt_ty(tcx, read_ty),
                            };
                            // Optimized MIR frequently materializes pointer-valued loads that are
                            // only metadata/provenance plumbing (e.g. forwarding `&&T` / raw ptr
                            // values through temporaries). Instrumenting those as memory READs can
                            // misclassify them as data accesses and produce false OOB/stale reports.
                            // We only instrument deref reads when the loaded value is non-pointer data.
                            let skip_pointer_value_read = self.is_pointer_ty(loaded_ty);
                            if skip_fn_ptr_read || skip_vtable_field_read || skip_pointer_value_read {
                                // Skip only the READ instrumentation; continue scanning this stmt.
                            } else {
                                let size_op = self.size_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
                                    loaded_ty,
                                    stmt.source_info.span,
                                );

                                self.ensure_raw_root_before(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    stmt.source_info,
                                    ptr_local,
                                    insert_points,
                                    ptr_locals_needing_tag,
                                    tagged_ptr_locals,
                                    ptr_locals_with_tag_sources,
                                );
                                ptr_locals_needing_tag.insert(ptr_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: true,
                                    source_info: stmt.source_info,
                                    place: p.clone(),
                                    kind: InstrKind::PtrRead { ptr_local, size_op },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Pointer write: any assignment whose LHS place begins with a Deref projection.
        if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
            let is_deref_write = lhs_place
                .projection
                .iter()
                .next()
                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
            if is_deref_write {
                let ptr_local = lhs_place.local;
                let ptr_ty = body.local_decls[ptr_local].ty;
                // Skip writes through &'static references. They are immutable by type,
                // and we don't track global memory for validity.
                let skip_static_ref_write =
                    matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                let skip_vtable_write = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                if !skip_static_ref_write && !skip_vtable_write && self.is_pointer_ty(ptr_ty) {
                    // Best-effort size: use the type of the *place being written* (after projections).
                    // This yields the correct size for patterns like `(*p).field = ...` or `(*p)[i] = ...`.
                    let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                    let size_op = self.size_operand_for_deref(
                        tcx,
                        body,
                        ptr_local,
                        lhs_ty,
                        stmt.source_info.span,
                    );

                    self.ensure_raw_root_before(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        ptr_local,
                        insert_points,
                        ptr_locals_needing_tag,
                        tagged_ptr_locals,
                        ptr_locals_with_tag_sources,
                    );
                    ptr_locals_needing_tag.insert(ptr_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: true,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::PtrWrite { ptr_local, size_op },
                    });
                }
            }
        }

        // Tag propagation across pointer-to-pointer casts and plain copies or moves of pointer locals.
        // Include wide pointers so tags survive unsize and reborrow patterns before a thin data
        // pointer is extracted later in MIR.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_pointer_ty(dst_ty) {
                    let mut skip_tag_prop = false;

                    // Casts to raw pointers should create a fresh raw tag with parent lineage,
                    // rather than copying the source tag directly.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr
                        | CastKind::PointerCoercion(_, _)
                        | CastKind::Transmute,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        if let TyKind::RawPtr(_pointee, mutbl) = dst_ty.kind() {
                            if let Some(src_place) = self.place_from_operand(op) {
                                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                if self.is_pointer_ty(src_ty) {
                                    let is_mut = matches!(mutbl, Mutability::Mut);
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_place.local);
                                    tagged_ptr_locals.insert(dst_local);
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place.clone(),
                                        },
                                    });
                                    skip_tag_prop = true;
                                }
                            }
                        }
                    }

                    // Pointer-from-non-pointer casts (including exposed provenance and transmute)
                    // drop lineage unless we synthesize a fresh root tag for the destination.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr
                        | CastKind::PointerCoercion(_, _)
                        | CastKind::Transmute
                        | CastKind::PointerWithExposedProvenance,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        let src_ty = op.ty(&body.local_decls, tcx);
                        if !self.is_pointer_ty(src_ty) && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RawRoot { ptr_local: dst_local, is_mut },
                            });
                            skip_tag_prop = true;
                        }
                    }
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local()),
                        // CopyForDeref shows up when MIR materializes a place for deref;
                        // it still represents a pointer value that needs tag propagation.
                        Rvalue::CopyForDeref(p) => p.as_local(),
                        // Wide-pointer construction can appear as aggregate from (data_ptr, metadata),
                        // e.g. `*mut [T] from (copy _data, copy _len)`. Preserve lineage from field 0.
                        Rvalue::Aggregate(_kind, ops) => ops
                            .iter()
                            .next()
                            .and_then(|op| self.place_from_operand(op))
                            .and_then(|p| p.as_local()),
                        // Pointer arithmetic lowering can appear as `Offset` binary ops.
                        Rvalue::BinaryOp(BinOp::Offset, ops) => self
                            .place_from_operand(&ops.0)
                            .and_then(|p| p.as_local()),
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute,
                            op,
                            _to_ty,
                        ) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        _ => None,
                    };

                    let mut src_local_opt = src_local_opt;
                    if src_local_opt.is_none() {
                        let projected_ptr_place = match rvalue {
                            Rvalue::Use(op) => self.place_from_operand(op),
                            Rvalue::BinaryOp(BinOp::Offset, ops) => self.place_from_operand(&ops.0),
                            Rvalue::Cast(
                                CastKind::PtrToPtr
                                | CastKind::PointerCoercion(_, _)
                                | CastKind::Transmute
                                | CastKind::PointerWithExposedProvenance,
                                op,
                                _,
                            ) => self.place_from_operand(op),
                            _ => None,
                        };
                        if let Some(p) = projected_ptr_place {
                            src_local_opt = self.recover_pointer_source_local_for_projected_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                p,
                                false,
                            );
                        }
                    }

                    if !skip_tag_prop {
                        if let Some(src_local) = src_local_opt {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                ptr_locals_needing_tag.insert(src_local);

                                let rhs_requires_retag = match rvalue {
                                    // Pointer arithmetic and projection-heavy pointer materialization
                                    // can change the pointee address; copying the source tag directly
                                    // keeps stale pointee metadata. Retag derived values instead.
                                    Rvalue::BinaryOp(op, _)
                                        if matches!(*op, BinOp::Offset | BinOp::Add | BinOp::Sub) =>
                                    {
                                        true
                                    }
                                    Rvalue::CopyForDeref(_)
                                    | Rvalue::Cast(
                                        CastKind::PtrToPtr
                                        | CastKind::PointerCoercion(_, _)
                                        | CastKind::Transmute,
                                        _,
                                        _,
                                    )
                                    | Rvalue::Aggregate(_, _) => true,
                                    Rvalue::Use(op) => match op {
                                        Operand::Copy(p) | Operand::Move(p) => !p.projection.is_empty(),
                                        _ => false,
                                    },
                                    _ => false,
                                };

                                if rhs_requires_retag {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::PtrDerive {
                                            dst: dst_local,
                                            src: src_local,
                                            is_mut: self.ptr_is_mut(dst_ty),
                                            is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                        },
                                    });
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::TagProp {
                                            dst: dst_local,
                                            src: src_local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                    });
                                }
                                tagged_ptr_locals.insert(dst_local);
                            }
                        } else {
                        // If the RHS is a projected place, there may be no pointer local we can
                        // propagate from, but the destination still needs a tag for later derefs.
                        // Synthesize a fresh root tag so the runtime does not see UNKNOWN_TAG.
                        let rhs_is_projected_ptr = match rvalue {
                            Rvalue::Use(op) => match op {
                                Operand::Copy(p) | Operand::Move(p) => {
                                    self.is_pointer_ty(p.ty(&body.local_decls, tcx).ty)
                                }
                                _ => false,
                            },
                            Rvalue::CopyForDeref(_p) => {
                                // If CopyForDeref produces a thin pointer local but we cannot
                                // propagate from a source local, synthesize a root tag for it.
                                // The destination type check below guards against non-pointers.
                                true
                            }
                            _ => false,
                        };

                        if matches!(rvalue, Rvalue::CopyForDeref(_)) && self.log_enabled(PassLogLevel::Trace) {
                            rz_pass_trace!(
                                self,
                                "CopyForDeref dst_local={:?} dst_ty={:?} rhs_is_projected_ptr={}",
                                dst_local,
                                dst_ty,
                                rhs_is_projected_ptr
                            );
                        }

                            if rhs_is_projected_ptr {
                                ptr_locals_needing_tag.insert(dst_local);
                                // The destination local is initialized by this assignment.
                                // Synthesize a root tag *after* the assignment so later uses
                                // (including call operands) do not see UNKNOWN_TAG.
                                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                    let is_mut = self.ptr_is_mut(dst_ty);
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx: stmt_idx + 1,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::RawRoot { ptr_local: dst_local, is_mut },
                                    });
                                    tagged_ptr_locals.insert(dst_local);
                                }
                            } else {
                            // If the RHS is a global/promoted pointer constant, record its allocation
                            // and synthesize a root tag for the destination.
                            let const_op: Option<&ConstOperand<'tcx>> = match rvalue {
                                Rvalue::Use(Operand::Constant(c)) => Some(c),
                                Rvalue::Cast(_, op, _) => match op {
                                    Operand::Constant(c) => Some(c),
                                    _ => None,
                                },
                                _ => None,
                            };

                                if let Some(c) = const_op {
                                    if let Some(info) = self.const_alloc_info(tcx, c) {
                                        let is_mut = self.ptr_is_mut(dst_ty);

                                        ptr_locals_needing_tag.insert(dst_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ConstAlloc {
                                                ptr_local: dst_local,
                                                size: info.size,
                                                base_offset: info.base_offset,
                                            },
                                        });
                                        if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx: stmt_idx + 1,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: Place::from(dst_local),
                                                kind: InstrKind::RawRoot {
                                                    ptr_local: dst_local,
                                                    is_mut,
                                                },
                                            });
                                            tagged_ptr_locals.insert(dst_local);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Handle pointer constants embedded in aggregate/field assignments where the destination
        // is not a pointer local we can tag. We still want to record the backing global allocation
        // so later derefs (e.g., vtable loads) do not report WILD_POINTER.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            let dst_is_local = dst_place.as_local().is_some();
            let mut const_ops: Vec<&ConstOperand<'tcx>> = Vec::new();

            match rvalue {
                Rvalue::Aggregate(_, ops) => {
                    for op in ops.iter() {
                        if let Operand::Constant(c) = op {
                            const_ops.push(c);
                        }
                    }
                }
                Rvalue::Use(Operand::Constant(c)) | Rvalue::Cast(_, Operand::Constant(c), _) => {
                    if !dst_is_local {
                        const_ops.push(c);
                    }
                }
                _ => {}
            }

            if !const_ops.is_empty() {
                let place_local = dst_place.as_local().unwrap_or(RETURN_PLACE);
                for c in const_ops {
                    if let Some(info) = self.const_alloc_info(tcx, c) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(place_local),
                            kind: InstrKind::ConstAllocConst {
                                const_op: c.clone(),
                                size: info.size,
                                base_offset: info.base_offset,
                            },
                        });
                    }
                }
            }
        }

        // std/alloc pattern where a thin pointer is produced by `Transmute` from
        // `NonNull<T>`/`Unique<T>` (ADT). TagProp does not apply because the source is not
        // necessarily a thin pointer local. Prefer deriving from a recovered pointer source
        // local (to keep lineage), and fall back to RawRoot only when recovery fails.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    if let Rvalue::Cast(CastKind::Transmute, op, _to_ty) = rvalue {
                        let src_ty = op.ty(body, tcx);
                        let is_nonnull_like = match src_ty.kind() {
                            TyKind::Adt(adt, _) => {
                                let name = tcx.def_path_str(adt.did());
                                name.contains("::ptr::NonNull")
                                    || name.contains("::ptr::Unique")
                                    || name.contains("core::ptr::NonNull")
                                    || name.contains("alloc::ptr::Unique")
                                    || name.contains("std::ptr::Unique")
                            }
                            _ => false,
                        };

                        if is_nonnull_like {
                            let is_mut = match dst_ty.kind() {
                                TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                _ => false,
                            };
                            let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                            let src_local_opt = self
                                .place_from_operand(op)
                                .and_then(|p| p.as_local())
                                .and_then(|src_local| {
                                    if self.is_pointer_ty(body.local_decls[src_local].ty) {
                                        Some(src_local)
                                    } else {
                                        self.backtrack_pointer_source_local(
                                            body,
                                            src_local,
                                            &block_data.statements[..stmt_idx],
                                        )
                                    }
                                });

                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            if let Some(src_local) = src_local_opt {
                                ptr_locals_needing_tag.insert(src_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::PtrDerive {
                                        dst: dst_local,
                                        src: src_local,
                                        is_mut,
                                        is_ref,
                                    },
                                });
                            } else {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot { ptr_local: dst_local, is_mut },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Fresh tag on pointer arithmetic: derived pointers (add/sub/offset) get a new tag
        // with parent linkage to the base pointer tag.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    let (binop, lhs_op) = match rvalue {
                        Rvalue::BinaryOp(op, box (lhs, _rhs)) => (Some(*op), Some(lhs)),
                        // Newer nightlies no longer have `Rvalue::CheckedBinaryOp`. The checked/overflowing
                        // forms lower to regular `BinaryOp` + extra logic, so handling `BinaryOp` is enough
                        // for our pointer-derive tagging purposes here.
                        _ => (None, None),
                    };

                    if let (Some(op), Some(lhs)) = (binop, lhs_op) {
                        // Raw pointer arithmetic frequently lowers to `BinOp::Offset`.
                        // Treat it like Add/Sub for tag-derivation so the derived pointer
                        // local does not remain untagged.
                        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Offset) {
                            if let Some(src_place) = self.place_from_operand(lhs) {
                                let src_local = src_place.local;
                                let src_ty = body.local_decls[src_local].ty;
                                if self.is_thin_ptr_ty(tcx, body, src_ty) {
                                    let is_mut = match dst_ty.kind() {
                                        TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                        TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                        _ => false,
                                    };

                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);
                                    tagged_ptr_locals.insert(dst_local);

                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::PtrDerive {
                                            dst: dst_local,
                                            src: src_local,
                                            is_mut,
                                            is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                        },
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Ref creation
        if let StatementKind::Assign(box (place, Rvalue::Ref(_, bk, src_place))) = &stmt.kind {
            if let Some(lhs_local) = place.as_local() {
                let lhs_ty = body.local_decls[lhs_local].ty;
                if self.is_pointer_ty(lhs_ty)
                    && !summary_elidable_shared_call_ref_locals.contains(&lhs_local)
                {
                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Ref { bk: *bk, src: src_place.clone() },
                    });
                }
            }
        }

        // Raw pointer creation
        if let StatementKind::Assign(box (place, Rvalue::RawPtr(mutbl, src_place))) = &stmt.kind {
            if let Some(lhs_local) = place.as_local() {
                let lhs_ty = body.local_decls[lhs_local].ty;
                if self.is_pointer_ty(lhs_ty) {
                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    let is_mut = matches!(*mutbl, RawPtrKind::Mut);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Raw { is_mut, src: src_place.clone() },
                    });
                }
            }
        }

    }




    /// Centralized call-effect classifier ("table").
    ///
    /// This MUST be kept consistent with instrumentation emission so that
    /// `warn_unknown_call_if_needed` does not drift from actual handling.
    fn classify_call_effect(&self, def_path: &str) -> CallEffect {
        if def_path.contains("decompress::Decompressor")
            && def_path.ends_with("::is_done")
        {
            return CallEffect::Ignore;
        }
        if self.is_box_into_raw_wrapper(def_path) {
            return CallEffect::BoxIntoRaw;
        }
        if self.is_box_from_raw_wrapper(def_path) {
            return CallEffect::BoxFromRaw;
        }

        if let Some(eff) = self.match_call_effect_rule(def_path) {
            return eff;
        }

        CallEffect::Unknown
    }

    /// Best-effort: compute byte size operand for memory ops given a pointer local and a count operand.
    ///
    /// Semantics: byte_len = count * size_of::<T>(), where `count` is in *elements*.
    ///
    /// Policy:
    /// - For thin pointers to sized types, emit `count * size_of::<T>()` as a MIR expression.
    /// - Otherwise, return a constant 0 (unknown).
    fn memop_size_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        count_op: &Operand<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_thin_ptr_ty(tcx, body, ptr_ty) {
            return self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, span);
        }

        let elem_ty = match ptr_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => *pointee_ty,
            TyKind::Ref(_, pointee_ty, _) => *pointee_ty,
            _ => {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
        };

        if !elem_ty.is_sized(tcx, body.typing_env(tcx)) {
            // TODO(wide-ptr): support unsized element types by using metadata length when available.
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }

        SizeOperand::ElemCount {
            elem_ty,
            count_op: count_op.clone(),
        }
    }

    fn materialize_size_operand<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        size_op: &SizeOperand<'tcx>,
    ) -> (Operand<'tcx>, Vec<Statement<'tcx>>) {
        match size_op {
            SizeOperand::Const(op) => (op.clone(), Vec::new()),
            SizeOperand::SizeOf(ty) => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(size_local)), vec![stmt])
            }
            SizeOperand::ElemCount { elem_ty, count_op } => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((Operand::Copy(Place::from(size_local)), count_op.clone())),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataSlice { ptr_local, elem_ty } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(
                            UnOp::PtrMetadata,
                            Operand::Copy(Place::from(*ptr_local)),
                        ),
                    ))),
                );
                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(size_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![meta_stmt, size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataStr { ptr_local } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(
                            UnOp::PtrMetadata,
                            Operand::Copy(Place::from(*ptr_local)),
                        ),
                    ))),
                );
                (Operand::Copy(Place::from(meta_local)), vec![meta_stmt])
            }
        }
    }



    /// Recognize std/alloc Box wrappers that return a raw pointer but take an ADT (Box<T>) as input.
    ///
    /// In optimized MIR, `Box::into_raw` appears as a direct call where the argument is an ADT,
    /// so we cannot use TagProp (arg0 is not a thin pointer local). We therefore synthesize a root
    /// raw-pointer tag for the returned pointer local.
    fn is_box_into_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::into_raw")
    }

    fn is_box_from_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::from_raw")
    }


    fn direct_callee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        func: &Operand<'tcx>,
    ) -> Option<(DefId, u64)> {
        let mut def_id_opt: Option<DefId> = None;

        if let TyKind::FnDef(callee_def_id, args) = func.ty(body, tcx).kind() {
            def_id_opt = Some(self.resolve_instance_def_id(tcx, body, *callee_def_id, args));
        } else if let Operand::Constant(c) = func {
            // Some direct calls come through a function pointer constant.
            def_id_opt = self.const_fn_def_id(tcx, body, c);
        } else if let Operand::Copy(p) | Operand::Move(p) = func {
            let ty = p.ty(&body.local_decls, tcx).ty;
            if let TyKind::FnDef(callee_def_id, args) = ty.kind() {
                def_id_opt = Some(self.resolve_instance_def_id(tcx, body, *callee_def_id, args));
            } else if matches!(ty.kind(), TyKind::FnPtr(..)) {
                def_id_opt = self.backtrack_fn_ptr_def_id(tcx, body, block_data, p.local);
            }
        }

        def_id_opt.map(|def_id| (def_id, self.callee_id_u64(tcx, def_id)))
    }

    /// Best-effort: recover the pointer source local from a call argument.
    ///
    /// Most pointer-derivation wrappers carry provenance in arg0, but some trait-based helpers
    /// (notably `SliceIndex::index{,_mut}`) take the pointer-bearing slice in arg1 and use arg0
    /// for an index/range value.
    fn call_arg_pointer_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        arg_index: usize,
    ) -> Option<Local> {
        let first = args.get(arg_index)?;
        let arg_place = self.place_from_operand(&first.node)?;
        let arg_local = arg_place.local;
        let arg_ty = body.local_decls[arg_local].ty;
        if self.is_pointer_ty(arg_ty) {
            return Some(arg_local);
        }
        if !arg_place.projection.is_empty() {
            for prefix_len in (0..arg_place.projection.len()).rev() {
                let prefix = PlaceRef {
                    local: arg_place.local,
                    projection: &arg_place.projection[..prefix_len],
                }
                .to_place(tcx);
                let prefix_ty = prefix.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(prefix_ty) {
                    return Some(prefix.local);
                }
            }
        }
        if let Some(src_local) =
            self.backtrack_pointer_source_local(body, arg_local, &block_data.statements)
        {
            return Some(src_local);
        }
        let base_local = self.backtrack_unsize_base_local(arg_local, &block_data.statements)?;
        if self.is_pointer_ty(body.local_decls[base_local].ty) {
            Some(base_local)
        } else {
            None
        }
    }

    /// Recover a pointer lineage source when a call writes an aggregate result into `agg_local`
    /// and a successor block later extracts a pointer field from that aggregate.
    ///
    /// Narrow shape handled:
    /// - predecessor terminator is a call whose `destination.local == agg_local`
    /// - call target is the current block
    /// - among the call arguments there is exactly one recoverable pointer source local
    ///
    /// This covers wrappers like `Result<&T, E>` where MIR stores the aggregate result in a
    /// non-pointer local and a later `_dst = move ((_ret as Ok).0)` would otherwise lose the
    /// original parent lineage and fall back to `RawRoot`.
    fn backtrack_single_pointer_arg_call_result_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        agg_local: Local,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for pred_bb in body.basic_blocks.indices() {
            let pred_data = &body.basic_blocks[pred_bb];
            let Some(term) = &pred_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args,
                destination,
                target,
                ..
            } = &term.kind
            else {
                continue;
            };

            if *target != Some(bb) || destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut ptr_sources = HashSet::new();
            for arg_index in 0..args.len() {
                if let Some(src_local) = self.call_arg_pointer_source_local(
                    tcx,
                    body,
                    pred_data,
                    args,
                    arg_index,
                ) {
                    ptr_sources.insert(src_local);
                    if ptr_sources.len() > 1 {
                        return None;
                    }
                }
            }

            let Some(src_local) = ptr_sources.into_iter().next() else {
                return None;
            };

            match recovered {
                Some(existing) if existing != src_local => return None,
                Some(_) => {}
                None => recovered = Some(src_local),
            }
        }

        if matched_call { recovered } else { None }
    }

    fn ptr_derive_source_arg_index(&self, def_path: &str) -> usize {
        if def_path.contains("SliceIndex")
            && (def_path.ends_with("::index") || def_path.ends_with("::index_mut"))
        {
            1
        } else if def_path.contains("::slice::index::<impl")
            && (def_path.ends_with("::index") || def_path.ends_with("::index_mut"))
        {
            1
        } else {
            0
        }
    }

    fn push_ptr_derive_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        src_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        tagged_ptr_locals: &mut HashSet<Local>,
        classified_derive_ptr_local: &mut Option<Local>,
    ) {
        // This call derives a new pointer from `src_local` (e.g. add/sub/offset/as_ptr).
        // We will emit a PtrDerive hook for the result, so suppress the redundant coarse PtrUse
        // for the base pointer argument.
        *classified_derive_ptr_local = Some(src_local);

        // Fresh tag derived from the base pointer tag.
        let is_mut = match dst_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        };
        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

        // NOTE: for ptr-derivation wrappers (add/sub/offset/...), the destination local
        // is only initialized *after* the call returns. We must therefore insert the PtrDerive
        // hook in the call's `target` block, not in the call block itself, otherwise we
        // expose provenance of an uninitialized local and record a garbage pointee address.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        tagged_ptr_locals.insert(dst_local);
        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                },
            });
        } else {
            // Fallback (should not happen for normal calls): keep the old placement.
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                },
            });
        }
    }

    fn push_box_into_raw_call<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        // Special-case: Box::into_raw returns a thin pointer derived from a Box ADT argument.
        // Since arg0 is not a thin pointer local, TagProp cannot apply; synthesize a root tag.
        let is_mut = self.ptr_is_mut(dst_ty);

        // Best-effort heap range recording for Box<T>: the raw pointer points to the T allocation.
        // TODO: hook real allocator shims/drop glue to get exact layout/size in general.
        let size_op: SizeOperand<'tcx> = match dst_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => {
                self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
            }
            TyKind::Ref(_, pointee_ty, _) => {
                self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
            }
            _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
        };

        // Insert after the call returns (in the call target block), so dst has the real value.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::RawRoot {
                    ptr_local: dst_local,
                    is_mut,
                },
            });
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::HeapAlloc {
                    ptr_local: dst_local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
        } else {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::RawRoot {
                    ptr_local: dst_local,
                    is_mut,
                },
            });
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::HeapAlloc {
                    ptr_local: dst_local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
        }
    }

    fn warn_unknown_call_if_needed<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        callee_path_opt: Option<&str>,
        callee_instrumented: bool,
        call_effect_opt: Option<CallEffect>,
    ) {
        if !self.warn_unknown_calls_enabled() {
            return;
        }

        if let Some(def_path) = callee_path_opt {
            if !def_path.starts_with("core::") && !def_path.starts_with("std::") {
                return;
            }

            // Does the call take any pointer argument?
            let mut has_ptr_arg = false;
            for a in args.iter() {
                if let Some(p) = self.place_from_operand(&a.node) {
                    let ty = body.local_decls[p.local].ty;
                    if self.is_pointer_ty(ty) {
                        has_ptr_arg = true;
                        break;
                    }
                }
            }

            // Does the call return a pointer into a local?
            let returns_ptr = destination
                .as_local()
                .is_some_and(|dl| self.is_pointer_ty(body.local_decls[dl].ty));

            if (has_ptr_arg || returns_ptr) && !callee_instrumented {
                // Use the centralized classifier so warning suppression matches actual handling.
                let effect =
                    call_effect_opt.unwrap_or_else(|| self.classify_call_effect(def_path));
                let known = !matches!(effect, CallEffect::Unknown);
                let trace_enabled = std::env::var("RZ_TRACE_CLASSIFY")
                    .ok()
                    .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
                    || self.log_enabled(PassLogLevel::Trace);
                if trace_enabled {
                    static TRACE_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    let filter = std::env::var("RZ_TRACE_CLASSIFY_FILTER").ok();
                    let limit = std::env::var("RZ_TRACE_CLASSIFY_LIMIT")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(50);
                    let mut count = TRACE_COUNT.get_or_init(|| Mutex::new(0)).lock().unwrap();
                    if *count < limit
                        && filter
                            .as_ref()
                            .map_or(true, |f| def_path.contains(f))
                    {
                        *count += 1;
                        rz_pass_warn!(
                            self,
                            "[rusteze][trace] classify_call_effect: {} => {:?} (filter={})",
                            def_path,
                            effect,
                            filter.as_deref().unwrap_or("<none>")
                        );
                    }
                }
                if trace_enabled && !known {
                    static TRACE_UNKNOWN_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    let limit = std::env::var("RZ_TRACE_CLASSIFY_UNKNOWN_LIMIT")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(20);
                    let mut count = TRACE_UNKNOWN_COUNT
                        .get_or_init(|| Mutex::new(0))
                        .lock()
                        .unwrap();
                    if *count < limit {
                        *count += 1;
                        let contains_slice_impl = def_path.contains("::slice::<impl [");
                        let ends_get = def_path.ends_with("::get");
                        let ends_get_mut = def_path.ends_with("::get_mut");
                        let ends_is_empty = def_path.ends_with("::is_empty");
                        rz_pass_warn!(
                            self,
                            "[rusteze][trace] unknown_call def_path={:?} contains_slice_impl={} ends_get={} ends_get_mut={} ends_is_empty={}",
                            def_path,
                            contains_slice_impl,
                            ends_get,
                            ends_get_mut,
                            ends_is_empty
                        );
                    }
                }
                if !known {
                    self.warn_unknown_call_once(def_path);
                }
            }
        }
    }

    fn push_memop_call_effects<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        is_copy: bool,
        is_memset: bool,
        classified_write_ptr_local: &mut Option<Local>,
        classified_read_ptr_local: &mut Option<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
    ) {
        if !is_copy && !is_memset {
            return;
        }

        if is_copy {
            // Signature convention we assume (matches core::intrinsics and ptr wrappers):
            //   copy::<T>(src: *const T, dst: *mut T, count: usize)
            //   copy_nonoverlapping::<T>(src: *const T, dst: *mut T, count: usize)
            if args.len() >= 3 {
                let src_place = args.get(0).and_then(|a| self.place_from_operand(&a.node));
                let dst_place = args.get(1).and_then(|a| self.place_from_operand(&a.node));
                let src_local = src_place.and_then(|p| {
                    self.resolve_ptr_local_for_call_place(tcx, body, block_data, p)
                });
                let dst_local = dst_place.and_then(|p| {
                    self.resolve_ptr_local_for_call_place(tcx, body, block_data, p)
                });
                let count_op = &args[2].node;

                let size_op_for = |ptr_local: Local| -> SizeOperand<'tcx> {
                    self.memop_size_bytes(tcx, body, ptr_local, count_op, term.source_info.span)
                };

                if let Some(src) = src_local {
                    *classified_read_ptr_local = Some(src);
                    ptr_locals_needing_tag.insert(src);
                    let size_op = size_op_for(src);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: src_place.unwrap_or(Place::from(src)),
                        kind: InstrKind::PtrRead { ptr_local: src, size_op },
                    });
                }
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size_op = size_op_for(dst);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: dst_place.unwrap_or(Place::from(dst)),
                        kind: InstrKind::PtrWrite { ptr_local: dst, size_op },
                    });
                }
            }
        } else if is_memset {
            // Signature convention:
            //   write_bytes::<T>(dst: *mut T, val: u8, count: usize)
            if args.len() >= 3 {
                let dst_place = args.get(0).and_then(|a| self.place_from_operand(&a.node));
                let dst_local = dst_place.and_then(|p| {
                    self.resolve_ptr_local_for_call_place(tcx, body, block_data, p)
                });
                let count_op = &args[2].node;
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size_op = self.memop_size_bytes(tcx, body, dst, count_op, term.source_info.span);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: dst_place.unwrap_or(Place::from(dst)),
                        kind: InstrKind::PtrWrite { ptr_local: dst, size_op },
                    });
                }
            }
        }
    }

    fn push_alloc_shim_effects<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        alloc_shim_kind: AllocShimKind,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        // Where to insert events that need the call's return value.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        match alloc_shim_kind {
            AllocShimKind::Alloc | AllocShimKind::AllocZeroed => {
                // Record the newly allocated pointer as live.
                if let Some(dst_local) = destination.as_local() {
                    let dst_ty = body.local_decls[dst_local].ty;
                    if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                        // Many std::alloc wrappers take a `Layout` as arg0 instead of (size, align).
                        // For Layout-taking forms we currently record unknown size=0.
                        // TODO: extract Layout.size so we can do precise OOB.
                        let size_op: Operand<'tcx> = if let Some(arg0) = args.get(0) {
                            let arg0_ty = arg0.node.ty(body, tcx);
                            match arg0_ty.kind() {
                                TyKind::Adt(adt, _) => {
                                    let name = tcx.def_path_str(adt.did());
                                    if name.contains("core::alloc::Layout")
                                        || name.contains("alloc::alloc::Layout")
                                        || name.contains("std::alloc::Layout")
                                    {
                                        self.const_usize(tcx, term.source_info.span, 0)
                                    } else {
                                        arg0.node.clone()
                                    }
                                }
                                _ => arg0.node.clone(),
                            }
                        } else {
                            self.const_usize(tcx, term.source_info.span, 0)
                        };
                        let size_op = SizeOperand::Const(size_op);

                        // Insert in the target block so `dst_local` is initialized.
                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::Dealloc => {
                // Deallocation: record pointer as dead.
                // For Layout-taking forms we currently record unknown size=0.
                if let Some(first) = args.get(0) {
                    if let Some(p) = self.place_from_operand(&first.node) {
                        let ptr_local = p.local;
                        let ptr_ty = body.local_decls[ptr_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, ptr_ty) {
                            let size_op: Operand<'tcx> = if args.len() >= 2 {
                                let arg1_ty = args[1].node.ty(body, tcx);
                                if matches!(arg1_ty.kind(), TyKind::Uint(_)) {
                                    args[1].node.clone()
                                } else {
                                    self.const_usize(tcx, term.source_info.span, 0)
                                }
                            } else {
                                self.const_usize(tcx, term.source_info.span, 0)
                            };
                            let size_op = SizeOperand::Const(size_op);

                            ptr_locals_needing_tag.insert(ptr_local);

                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(ptr_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local,
                                    live: false,
                                    size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::Realloc => {
                // Record old ptr dead, new ptr live. Signature: (ptr, old_size, align, new_size) -> *mut u8
                if let Some(first) = args.get(0) {
                    if let Some(p) = self.place_from_operand(&first.node) {
                        let old_ptr_local = p.local;
                        let old_ptr_ty = body.local_decls[old_ptr_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, old_ptr_ty) {
                            let old_size_op: Operand<'tcx> = if args.len() >= 2 {
                                let arg1_ty = args[1].node.ty(body, tcx);
                                if matches!(arg1_ty.kind(), TyKind::Uint(_)) {
                                    args[1].node.clone()
                                } else {
                                    self.const_usize(tcx, term.source_info.span, 0)
                                }
                            } else {
                                self.const_usize(tcx, term.source_info.span, 0)
                            };
                            let old_size_op = SizeOperand::Const(old_size_op);

                            ptr_locals_needing_tag.insert(old_ptr_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(old_ptr_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: old_ptr_local,
                                    live: false,
                                    size_op: old_size_op,
                                },
                            });
                        }
                    }
                }

                if let Some(dst_local) = destination.as_local() {
                    let dst_ty = body.local_decls[dst_local].ty;
                    if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                        let new_size_op: Operand<'tcx> = if args.len() >= 4 {
                            args[3].node.clone()
                        } else if args.len() >= 3 {
                            args[2].node.clone()
                        } else {
                            self.const_usize(tcx, term.source_info.span, 0)
                        };
                        let new_size_op = SizeOperand::Const(new_size_op);

                        ptr_locals_needing_tag.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op: new_size_op,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op: new_size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::No => {}
        }
    }

    fn scan_call_terminator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        func: &Operand<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        let callee_opt = self.direct_callee(tcx, body, block_data, func);
        let callee_id_opt = callee_opt.map(|(_did, cid)| cid);
        let callee_path_opt = callee_opt.map(|(did, _)| tcx.def_path_str(did));
        let callee_instrumented = callee_opt
            .map(|(did, _)| self.is_instrumented_callee(tcx, did))
            .unwrap_or(false);
        let ret_take_enabled = self.ret_take_enabled();

        // 6a: Remove is_plain_store/is_plain_load computation.

        // Centralized effect classification for direct calls.
        let mut call_effect_opt: Option<CallEffect> = callee_path_opt.as_deref().map(|p| self.classify_call_effect(p));
        let unknown_call = !callee_instrumented
            && matches!(call_effect_opt, None | Some(CallEffect::Unknown));

        // Unknown-call warnings are suppressed now that we instrument all non-std crates.

        let mut classified_write_ptr_local: Option<Local> = None;
        let mut classified_read_ptr_local: Option<Local> = None;
        let mut classified_derive_ptr_local: Option<Local> = None;
        let unknown_call_returns_ptr =
            unknown_call && self.is_pointer_ty(destination.ty(&body.local_decls, tcx).ty);
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };
        // Caller-side writeback retag set:
        // if a callee receives `&mut P` (where `P` is itself a pointer type), it may mutate
        // the caller's pointer local through that reference. Without a post-call retag, the
        // caller keeps using the stale pre-call tag for `P`, which can hide aliasing UB.
        //
        // Example:
        //   fn retarget(x: &mut &u32, t: &mut u32) { *x = &mut *(t as *mut _); }
        //   retarget(&mut target_alias, target);
        //   *target = 13;
        //   black_box(*target_alias); // must observe updated tag lineage.
        let mut post_call_writeback_retag_locals: HashSet<Local> = HashSet::new();

        // Centralized emission for direct-call effects.
        if let Some(effect) = call_effect_opt {
            match effect {
                CallEffect::Ignore => {
                    // No memory/pointer effect.
                }

                CallEffect::AllocShim(kind) => {
                    if self.heap_allocs_from_mir_enabled() {
                        // Old behavior: emit HeapAlloc hooks from MIR (may require Layout.size extraction).
                        self.push_alloc_shim_effects(
                            tcx,
                            body,
                            bb,
                            block_data,
                            term,
                            args,
                            destination,
                            kind,
                            insert_points,
                            ptr_locals_needing_tag,
                            tagged_ptr_locals,
                        );
                    } else {
                        // New default: rely on runtime global allocator wrapper for heap tracking.
                        // Still tag allocator-returned pointers so later READ/WRITE are not UNKNOWN_TAG.
                        let returns_ptr = matches!(
                            kind,
                            AllocShimKind::Alloc | AllocShimKind::AllocZeroed | AllocShimKind::Realloc
                        );

                        if returns_ptr {
                            if let Some(dst_local) = destination.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;

                                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                    ptr_locals_needing_tag.insert(dst_local);

                                    // IMPORTANT: destination local is initialized only after call returns.
                                    // Insert in call target block at stmt 0.
                                    let call_target_bb: Option<BasicBlock> = match &term.kind {
                                        TerminatorKind::Call { target, .. } => *target,
                                        _ => None,
                                    };

                                    if let Some(tgt_bb) = call_target_bb {
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                            },
                                        });
                                    } else {
                                        // Fallback: if no target, place at end of current block.
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                            },
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::MemCopy | CallEffect::MemSet => {
                    // Memcpy/memset-style operations (intrinsics and std/core wrappers).
                    // These are real READ/WRITE effects even when there is no explicit `(*p)` deref in MIR.
                    let is_copy = matches!(effect, CallEffect::MemCopy);
                    let is_memset = matches!(effect, CallEffect::MemSet);
                    self.push_memop_call_effects(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        args,
                        is_copy,
                        is_memset,
                        &mut classified_write_ptr_local,
                        &mut classified_read_ptr_local,
                        insert_points,
                        ptr_locals_needing_tag,
                    );
                }

                CallEffect::Store => {
                    // store wrapper/intrinsic: WRITE through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                classified_write_ptr_local = Some(ptr_local);
                                ptr_locals_needing_tag.insert(ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let size_op = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl) => self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        *pointee_ty,
                                        term.source_info.span,
                                    ),
                                    TyKind::Ref(_, pointee_ty, _mutbl) => self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        *pointee_ty,
                                        term.source_info.span,
                                    ),
                                    _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrWrite {
                                        ptr_local,
                                        size_op,
                                    },
                                });
                            }
                        }
                    }
                }

                CallEffect::Load => {
                    // load wrapper/intrinsic: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                classified_read_ptr_local = Some(ptr_local);
                                ptr_locals_needing_tag.insert(ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let size_op = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl) => self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        *pointee_ty,
                                        term.source_info.span,
                                    ),
                                    TyKind::Ref(_, pointee_ty, _mutbl) => self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        *pointee_ty,
                                        term.source_info.span,
                                    ),
                                    _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrRead {
                                        ptr_local,
                                        size_op,
                                    },
                                });
                            }
                        }
                    }
                }

                CallEffect::PtrDerive => {
                    // Pointer-result handling for ptr-derivation wrappers (add/sub/offset/as_ptr...).
                    // Only needed when the callee is not instrumented.
                    if !callee_instrumented {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            // Allow wide-pointer destinations too (e.g., from_raw_parts_mut -> &mut [T]).
                            if self.is_pointer_ty(dst_ty) {
                                let src_arg_index = callee_path_opt
                                    .as_deref()
                                    .map(|p| self.ptr_derive_source_arg_index(p))
                                    .unwrap_or(0);
                                if let Some(src_local) = self.call_arg_pointer_source_local(
                                    tcx,
                                    body,
                                    block_data,
                                    args,
                                    src_arg_index,
                                )
                                {
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);
                                    Self::push_ptr_derive_call(
                                        bb,
                                        block_data,
                                        term,
                                        dst_local,
                                        dst_ty,
                                        src_local,
                                        insert_points,
                                        tagged_ptr_locals,
                                        &mut classified_derive_ptr_local,
                                    );
                                }
                            }
                        }
                    }
                }

                CallEffect::BoxIntoRaw => {
                    // Box::into_raw boundary modeling: root-tag + HeapAlloc live.
                    if !callee_instrumented {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                self.push_box_into_raw_call(
                                    tcx,
                                    body,
                                    bb,
                                    block_data,
                                    term,
                                    dst_local,
                                    dst_ty,
                                    insert_points,
                                );
                            }
                        }
                    }
                }

                CallEffect::BoxFromRaw => {
                    // Box::from_raw only rewraps an existing allocation, so do not emit
                    // any heap lifetime event here. Pointer argument tagging happens
                    // through the regular call argument handling below.
                }

                CallEffect::Unknown => {
                    // No special emission here.
                }
            }
        }


        // Treat any pointer argument as tag relevant.
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else { continue; };
            let ty = body.local_decls[p.local].ty;
            if !self.is_pointer_ty(ty) { continue; }
            let is_addr_exposable = self.is_addr_exposable_ptr_ty(tcx, body, ty);

            // If this arg is `&mut P` (P pointer-typed), track the pointee local for
            // post-call retagging in the caller to avoid stale tags after writeback.
            if let TyKind::Ref(_, pointee_ty, mutbl) = ty.kind() {
                if matches!(mutbl, Mutability::Mut) && self.is_pointer_ty(*pointee_ty) {
                    if let Some(pointee_local) =
                        self.backtrack_mut_ref_pointee_local(p.local, &block_data.statements)
                    {
                        if self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                            post_call_writeback_retag_locals.insert(pointee_local);
                        }
                    }
                }
            }

            let was_tagged = tagged_ptr_locals.contains(&p.local);

            // Ensure a tag exists before any call-boundary effects that consume it.
            if !was_tagged {
                let is_mut = match ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let is_ref = matches!(ty.kind(), TyKind::Ref(..));
                tagged_ptr_locals.insert(p.local);
                ptr_locals_needing_tag.insert(p.local);
                let derive_src = self
                    .backtrack_pointer_source_local(body, p.local, &block_data.statements)
                    .filter(|src_local| *src_local != p.local && tagged_ptr_locals.contains(src_local));
                if let Some(src_local) = derive_src {
                    ptr_locals_needing_tag.insert(src_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        // Use the full argument place (including projections), not just
                        // the carrier local. For projected pointer arguments, using only
                        // `p.local` retags the wrong address and desynchronizes tag pointee
                        // from the actual call operand pointer value.
                        place: p,
                        kind: InstrKind::PtrDerive {
                            dst: p.local,
                            src: src_local,
                            is_mut,
                            is_ref,
                        },
                    });
                } else {
                    let root_kind = if is_ref {
                        // For reference-typed args, preserve ref semantics at the root.
                        // Emitting RawRoot here loses that information and can produce
                        // spurious stack wild-pointer reports when call-boundary tags
                        // are missing for optimized/indirect calls.
                        InstrKind::RetRoot {
                            dst_local: p.local,
                            is_mut,
                            is_ref: true,
                        }
                    } else {
                        InstrKind::RawRoot { ptr_local: p.local, is_mut }
                    };
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        // Use the full argument place (including projections), not just
                        // the carrier local. For projected pointer arguments, using only
                        // `p.local` retags the wrong address and desynchronizes tag pointee
                        // from the actual call operand pointer value.
                        place: p,
                        kind: root_kind,
                    });
                }
            }
        
            // Inter-procedural: push argument tag to callee if instrumented.
            if callee_instrumented {
                if let Some(callee_id) = callee_id_opt {
                    ptr_locals_needing_tag.insert(p.local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind: InstrKind::CallArgPush {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                        },
                    });
                }
            }

            // Unknown call policy: conservatively model potential read/write through any pointer arg.
            if unknown_call
                && was_tagged
                && is_addr_exposable
                && !unknown_call_returns_ptr
                && matches!(ty.kind(), TyKind::Ref(..))
            {
                let is_mut_ref = matches!(ty.kind(), TyKind::Ref(_, _, Mutability::Mut));
                let size_op = match ty.kind() {
                    TyKind::RawPtr(pointee_ty, _) | TyKind::Ref(_, pointee_ty, _) => {
                        self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
                    }
                    _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
                };
                ptr_locals_needing_tag.insert(p.local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind: InstrKind::PtrReadAllowUntagged {
                        ptr_local: p.local,
                        size_op: size_op.clone(),
                    },
                });
                // Shared refs (`&T`) are read-only at the type level. Emitting unknown-call
                // write checks for them causes false positives in std/core helper paths
                // (e.g., compare/equality intrinsics over static data).
                if is_mut_ref {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind: InstrKind::PtrWriteAllowUntagged { ptr_local: p.local, size_op },
                    });
                }
            }
        
            // Always record a coarse escape event for pointer arguments at call boundaries,
            // even when specific effects (read/write/derive) are also modeled.
            if self.filter_stdlib_uses_enabled() && self.span_is_stdlib(tcx, term.source_info.span) {
                continue;
            }
            ptr_locals_needing_tag.insert(p.local);
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: p,
                kind: InstrKind::PtrUse { ptr_local: p.local },
            });
        }

        // Caller-side writeback retag:
        // At call return, re-seed tags for pointer locals that may have been rewritten through
        // `&mut` pointer arguments. This keeps subsequent accesses tied to the updated pointer
        // value instead of the stale pre-call tag.
        if let Some(tgt_bb) = call_target_bb {
            let mut writeback_locals: Vec<Local> =
                post_call_writeback_retag_locals.into_iter().collect();
            writeback_locals.sort_by_key(|l| l.index());

            for dst_local in writeback_locals {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    continue;
                }
                let is_mut = match dst_ty.kind() {
                    TyKind::Ref(_, _, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                ptr_locals_needing_tag.insert(dst_local);
                tagged_ptr_locals.insert(dst_local);
                insert_points.push(InsertPoint {
                    bb: tgt_bb,
                    stmt_idx: 0,
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::RetRoot {
                        dst_local,
                        is_mut,
                        is_ref,
                    },
                });
            }
        }

        // Caller-side return-tag recovery for pointer returns.
        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.supports_call_boundary_ret_tag_ty(tcx, body, dst_ty) {
                if callee_instrumented {
                    if !ret_take_enabled {
                        let is_mut = match dst_ty.kind() {
                            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            _ => false,
                        };
                        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        }
                    } else if let Some(callee_id) = callee_id_opt {
                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::RetTake { callee_id, dst_local },
                        });
                    }
                } else {
                    // Uninstrumented callee: synthesize a fresh return tag unless another effect already
                    // models the return pointer (alloc shims/ptr-derive/Box::into_raw).
                    let alloc_returns_ptr = matches!(
                        call_effect_opt,
                        Some(CallEffect::AllocShim(
                            AllocShimKind::Alloc | AllocShimKind::AllocZeroed | AllocShimKind::Realloc
                        ))
                    );
                    let mut return_tagged_by_effect = matches!(
                        call_effect_opt,
                        Some(CallEffect::PtrDerive | CallEffect::BoxIntoRaw)
                    ) || (alloc_returns_ptr && !self.heap_allocs_from_mir_enabled());

                    // `core::intrinsics::read_via_copy` is classified as `Load`: when it returns
                    // a pointer value, that return is derived from arg0's pointer provenance.
                    if !return_tagged_by_effect
                        && matches!(call_effect_opt, Some(CallEffect::Load))
                    {
                        if let Some(src_local) =
                            self.call_arg_pointer_source_local(tcx, body, block_data, args, 0)
                        {
                            ptr_locals_needing_tag.insert(dst_local);
                            ptr_locals_needing_tag.insert(src_local);
                            Self::push_ptr_derive_call(
                                bb,
                                block_data,
                                term,
                                dst_local,
                                dst_ty,
                                src_local,
                                insert_points,
                                tagged_ptr_locals,
                                &mut classified_derive_ptr_local,
                            );
                            return_tagged_by_effect = true;
                        }
                    }

                    if !return_tagged_by_effect {
                        let is_mut = match dst_ty.kind() {
                            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            _ => false,
                        };
                        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

                        ptr_locals_needing_tag.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        }
                    }
                }
            }
        }

        let is_box_from_raw = callee_path_opt
            .as_deref()
            .is_some_and(|p| self.is_box_from_raw_wrapper(p));
        if is_box_from_raw {
            // Box::from_raw rewraps an existing allocation. Any dead HeapAlloc hook here
            // makes the destructor read look like use-after-dead (bytes::release_shared).
            insert_points.retain(|ip| {
                !(ip.bb == bb && matches!(ip.kind, InstrKind::HeapAlloc { live: false, .. }))
            });
        }
    }

    fn scan_body<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) -> ScanResult<'tcx> {
        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();
        let mut tagged_ptr_locals: HashSet<Local> = HashSet::new();
        let ptr_locals_with_tag_sources = self.collect_ptr_locals_with_tag_sources(tcx, body);
        let summary_elidable_shared_call_ref_locals =
            self.compute_summary_elidable_shared_call_ref_locals(tcx, body);

        let mut explicitly_tracked: HashSet<Local> = HashSet::new();
        for block_data in body.basic_blocks.iter() {
            for stmt in block_data.statements.iter() {
                match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        explicitly_tracked.insert(local);
                    }
                    _ => {}
                }
            }
        }

        let interesting_stack_locals = self.compute_interesting_stack_locals(tcx, body);

        // Fallback stack locals:
        // Some locals never get explicit `StorageLive/StorageDead` in optimized MIR,
        // including address-taken arguments. Example pattern:
        //   _2 = &_1;       // _1 is an argument
        //   _3 = copy (*_2);
        // Without a fallback stack alloc, the read from `_2` looks like a wild pointer.
        //
        // To handle this, we conservatively treat these locals as always live:
        //  - record a StackAlloc(live=true) at function entry
        //  - record a StackAlloc(live=false) at every return site
        //
        // This is a fallback mechanism; precise lifetime tracking via explicit
        // StorageLive/StorageDead takes precedence when available.
        let mut fallback_locals: Vec<(Local, SizeOperand<'tcx>)> = Vec::new();
        for local in body.local_decls.indices() {
            if local == RETURN_PLACE && !interesting_stack_locals.contains(&local) {
                continue;
            }
            if explicitly_tracked.contains(&local) {
                continue;
            }
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local) {
                continue;
            }
            let size_op = self.size_operand_for_stack_local_ty(tcx, body, ty, rustc_span::DUMMY_SP);
            if matches!(size_op, SizeOperand::Const(_)) {
                continue;
            }
            fallback_locals.push((local, size_op));
        }

        let track_all_stack_allocs = self.track_all_stack_allocs_flag();
          
        let entry_insert_at = self.entry_insert_after_prologue(body);
        self.push_arg_retags_at_entry(
            tcx,
            body,
            &mut insert_points,
            &mut ptr_locals_needing_tag,
            &mut tagged_ptr_locals,
            entry_insert_at,
        );

        let mut return_sites: Vec<(BasicBlock, SourceInfo, usize)> = Vec::new();

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                self.scan_statement(
                    tcx,
                    body,
                    bb,
                    block_data,
                    stmt_idx,
                    stmt,
                    &mut insert_points,
                    &mut ptr_locals_needing_tag,
                    &mut tagged_ptr_locals,
                    &ptr_locals_with_tag_sources,
                    &summary_elidable_shared_call_ref_locals,
                    &interesting_stack_locals,
                    track_all_stack_allocs,
                );
            }

            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call { func, args, destination, .. } = &term.kind {
                    self.scan_call_terminator(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        func,
                        args,
                        destination,
                        &mut insert_points,
                        &mut ptr_locals_needing_tag,
                        &mut tagged_ptr_locals,
                    );
                }

                if let TerminatorKind::Return = &term.kind {
                    let callee_id = self.callee_id_u64(tcx, body.source.def_id());
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(RETURN_PLACE),
                        kind: InstrKind::FnExit { callee_id },
                    });
                    if self.ret_push_enabled()
                        && self.supports_call_boundary_ret_tag_ty(tcx, body, body.return_ty())
                    {
                        ptr_locals_needing_tag.insert(RETURN_PLACE);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(RETURN_PLACE),
                            kind: InstrKind::RetPush { callee_id, ptr_local: RETURN_PLACE },
                        });
                    }
                    return_sites.push((bb, term.source_info, block_data.statements.len()));
                }
            }
        }

        // IMPORTANT ORDERING NOTE:
        // `StackAlloc` is implemented via terminator-splitting (calls in fresh blocks).
        // If we insert multiple terminator-splitting hooks at the same location, the last applied
        // hook will execute first.
        //
        // `insert_instrumentation` iterates `insert_points` in reverse, meaning:
        //   - earlier items in `insert_points` are applied later
        //   - and therefore execute earlier
        //
        // To ensure fallback entry alloc tracking runs at real function entry (after prologue) and
        // before other inserted hooks, we prepend these InsertPoints.
        let mut fallback_entry_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut fallback_return_points: Vec<InsertPoint<'tcx>> = Vec::new();

        for (local, size_op) in fallback_locals.iter().cloned() {
            if local == RETURN_PLACE && !interesting_stack_locals.contains(&local) {
                continue;
            }

            // Only track stack slots that are actually address-taken (unless user forces all).
            if !track_all_stack_allocs && !interesting_stack_locals.contains(&local) {
                continue;
            }

            // Record pointer-typed locals only if their address is taken.
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local) {
                continue;
            }

            fallback_entry_points.push(InsertPoint {
                bb: START_BLOCK,
                stmt_idx: entry_insert_at,
                // Insert after rustc's StorageLive prologue statements.
                insert_before: false,
                source_info: SourceInfo {
                    span: rustc_span::DUMMY_SP,
                    scope: OUTERMOST_SOURCE_SCOPE,
                },
                place: Place::from(local),
                kind: InstrKind::StackAlloc {
                    local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
            self.trace_stack_alloc_emit(
                tcx,
                body,
                local,
                true,
                &size_op,
                "FallbackEntry",
            );

            for (ret_bb, ret_source_info, ret_stmt_idx) in return_sites.iter().copied() {
                fallback_return_points.push(InsertPoint {
                    bb: ret_bb,
                    stmt_idx: ret_stmt_idx,
                    insert_before: false,
                    source_info: ret_source_info,
                    place: Place::from(local),
                    kind: InstrKind::StackAlloc {
                        local,
                        live: false,
                        size_op: size_op.clone(),
                    },
                });
                self.trace_stack_alloc_emit(
                    tcx,
                    body,
                    local,
                    false,
                    &size_op,
                    "FallbackReturn",
                );
            }
        }

        // Prepend entry fallback points so they are applied last and execute first.
        insert_points.splice(0..0, fallback_entry_points);
        // Append return points normally; they stay associated with return blocks.
        insert_points.extend(fallback_return_points);

        let mut insert_points = self.filter_insert_points_by_unsafe_dataflow(
            tcx,
            body,
            insert_points,
            unsafe_influence,
        );
        metadata_dataflow::apply_metadata_dataflow(self, body, &mut insert_points);

        ScanResult {
            insert_points,
            ptr_locals_needing_tag,
        }
    }

    fn allocate_tag_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        ptrs: HashSet<Local>,
    ) -> HashMap<Local, Local> {
        let mut tag_local_for_ptr_local: HashMap<Local, Local> = HashMap::new();
        for ptr_local in ptrs.into_iter() {
            if !tag_local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
                tag_local_for_ptr_local.insert(ptr_local, t);
            }
        }
        tag_local_for_ptr_local
    }

    fn init_tag_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        skip_ptr_locals: &HashSet<Local>,
    ) {
        // Initialize tag locals at function entry so we never read uninitialized tag values
        // (which would show up as `unknown tag=<garbage>` in the runtime).
        //
        // This does NOT solve inter-procedural tag passing/retagging by itself; it just ensures
        // the default is `0` ("untagged") rather than uninitialized memory.
        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for (ptr_local, tag_local) in tag_local_for_ptr_local.iter() {
            // Argument tags are set by ArgRetag at entry; avoid overwriting them with zero.
            if skip_ptr_locals.contains(ptr_local) {
                init_stmts.push(Statement::new(
                    source_info,
                    StatementKind::StorageLive(*tag_local),
                ));
                continue;
            }
            // Ensure the tag local is live, then initialize it to 0 ("untagged").
            init_stmts.push(Statement::new(source_info, StatementKind::StorageLive(*tag_local)));

            let zero: Operand<'tcx> = self.const_u64(tcx, source_info.span, 0);
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*tag_local),
                    Rvalue::Use(zero),
                ))),
            ));
        }

        // Insert right after the initial StorageLive prologue in the entry block.
        // This avoids reordering rustc's own prologue statements and ensures our locals
        // are considered live before we assign to them.
        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[entry_bb];

        let mut insert_at = 0usize;
        while insert_at < bd.statements.len() {
            match bd.statements[insert_at].kind {
                StatementKind::StorageLive(_) => insert_at += 1,
                _ => break,
            }
        }

        bd.statements.splice(insert_at..insert_at, init_stmts);
    }

    fn func_operand_for<'tcx>(&self, tcx: TyCtxt<'tcx>, hooks: Hooks, kind: &InstrKind<'tcx>, sp: Span) -> Operand<'tcx> {
        let def_id = match kind {
            InstrKind::Ref { .. } => hooks.def_id_ref,
            InstrKind::Raw { .. } => hooks.def_id_raw,
            InstrKind::RawRoot { .. } => hooks.def_id_raw,
            InstrKind::RetRoot { is_ref, .. } => {
                if *is_ref {
                    hooks.def_id_ref
                } else {
                    hooks.def_id_raw
                }
            }
            InstrKind::StackAlloc { .. } => hooks.def_id_alloc,
            InstrKind::HeapAlloc { .. } => hooks.def_id_alloc,
            // ConstAlloc is recorded via the same allocation hook.
            InstrKind::ConstAlloc { .. } | InstrKind::ConstAllocConst { .. } => hooks.def_id_alloc,
            InstrKind::PtrWrite { .. } => hooks.def_id_write,
            InstrKind::PtrWriteAllowUntagged { .. } => hooks.def_id_write_allow_untagged,
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrReadAllowUntagged { .. } => hooks.def_id_read_allow_untagged,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::TagProp { .. } => hooks.def_id_use, // should never become a call (handled as a plain Assign)
            InstrKind::PtrDerive { is_ref, .. } => {
                if *is_ref { hooks.def_id_ref } else { hooks.def_id_raw }
            }
            InstrKind::CallArgPush { .. } => hooks.def_id_push_call_arg_tag,
            InstrKind::ArgRetag { .. } => hooks.def_id_take_call_arg_tag,
            InstrKind::RetPush { .. } => hooks.def_id_push_ret_tag,
            InstrKind::RetTake { .. } => hooks.def_id_take_ret_tag_or_root,
            InstrKind::FnExit { .. } => hooks.def_id_exit_fn,
        };
        Operand::function_handle(tcx, def_id, std::iter::empty(), sp)
    }

    fn insert_instrumentation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        insert_points: Vec<InsertPoint<'tcx>>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        hooks: Hooks,
    ) {
        fn instr_priority(kind: &InstrKind<'_>) -> u8 {
            match kind {
                InstrKind::Ref { .. }
                | InstrKind::Raw { .. }
                | InstrKind::RawRoot { .. }
                | InstrKind::ArgRetag { .. }
                | InstrKind::FnExit { .. }
                | InstrKind::RetRoot { .. }
                | InstrKind::PtrDerive { .. } => 0,
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. } => 1,
                InstrKind::CallArgPush { .. } | InstrKind::PtrUse { .. } => 2,
                _ => 3,
            }
        }

        // ArgRetag must run at function entry before any ptr reads/writes in the callee.
        // We split it out so we can enforce ordering independent of stmt_idx sorting.
        let mut arg_retag_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();
        let mut other_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();

        for (idx, ip) in insert_points.into_iter().enumerate() {
            if matches!(ip.kind, InstrKind::ArgRetag { .. }) {
                arg_retag_points.push((idx, ip));
            } else {
                other_points.push((idx, ip));
            }
        }

        let sort_points = |points: &mut Vec<(usize, InsertPoint<'tcx>)>| {
            points.sort_by_key(|(idx, ip)| {
                (
                    ip.bb.index(),
                    ip.stmt_idx,
                    instr_priority(&ip.kind),
                    *idx,
                )
            });
        };

        sort_points(&mut other_points);
        sort_points(&mut arg_retag_points);

        for (_idx, ip) in other_points.into_iter().rev() {
            let bb = ip.bb;
            let stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            // Avoid emitting allow-untagged READ/WRITE for raw-pointer args from unknown calls.
            // These are a common source of false positives (e.g., pointer casts).
            if let InstrKind::PtrReadAllowUntagged { ptr_local, .. }
                | InstrKind::PtrWriteAllowUntagged { ptr_local, .. } = creation_kind
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if matches!(ptr_ty.kind(), TyKind::RawPtr(..)) {
                    continue;
                }
            }

            // workaround for pointers produced from NonNull/Unique via Transmute
            // RawRoot lowering: we implement this by mirroring the existing Raw lowering code path:
            //   tag(ptr_local) = __record_raw_ptr_creation(expose(ptr_local), is_mut, 0)
            if let InstrKind::RawRoot { ptr_local, is_mut } = creation_kind.clone() {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if !self.is_pointer_ty(ptr_ty) {
                    continue;
                }
                let dst_tag = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RawRoot");

                // Compute exposed address first. This is fallible for some pointer shapes, and we must
                // not mutate CFG until we know RawRoot lowering can be emitted completely.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let Some((data_ptr_stmt_opt, addr_stmt)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(ptr_local),
                    addr_local,
                ) else {
                    continue;
                };
                let is_mut_u8: u8 = if is_mut { 1 } else { 0 };
                let alias_exempt = self.alias_exempt_for_ptr_ty(
                    tcx,
                    body,
                    body.local_decls[ptr_local].ty,
                );
                let bounds_len_op = self.bounds_len_operand_for_ptr_local(
                    tcx,
                    body,
                    ptr_local,
                    source_info.span,
                );
                let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                    tcx,
                    body,
                    source_info,
                    &bounds_len_op,
                );
                if !bounds_len_stmts.is_empty() {
                    // Appended after address statements once call_bb exists.
                }

                // We insert using the same “split block with a call terminator” style used elsewhere.
                // Create fresh block that will run the call and then continue.
                let is_cleanup = body.basic_blocks[bb].is_cleanup;

                // Split the current block at the correct position so RawRoot runs
                // before or after the target statement based on insert_before.
                let mut tail_stmts: Vec<Statement<'tcx>> = Vec::new();
                {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    tail_stmts.extend(bd.statements.drain(split_at..));
                }

                let orig_term = body.basic_blocks[bb].terminator.clone();
                let cont_bb = {
                    let mut cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    cont_data.statements = tail_stmts;
                    body.basic_blocks_mut().push(cont_data)
                };

                // Rewrite original terminator to jump to the new RawRoot call block.
                // Create the RawRoot call block and set it as the new successor.
                let call_bb = body.basic_blocks_mut().push(BasicBlockData::new(None, is_cleanup));
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto { target: call_bb },
                });

                if let Some(data_ptr_stmt) = data_ptr_stmt_opt {
                    body.basic_blocks_mut()[call_bb].statements.push(data_ptr_stmt);
                }
                body.basic_blocks_mut()[call_bb].statements.push(addr_stmt);
                if !bounds_len_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut bounds_len_stmts);
                }

                let raw_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_raw,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_raw: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, if alias_exempt { 1 } else { 0 }),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[call_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: raw_func,
                        args: args_raw,
                        destination: Place::from(dst_tag),
                        target: Some(cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Normal,
                        fn_span: source_info.span,
                    },
                });

                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    body.basic_blocks_mut()[cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    );
                }

                // Done handling this insert point.
                continue;
            }

            // Caller-side: take return tag after a call returned a pointer into `dst_local`.
            //
            // IMPORTANT: this must be per-call-edge, not per-target-block.
            // A single target block may have multiple call predecessors. If we patch the
            // target block in-place, we would incorrectly run one call's RetTake logic for
            // all predecessors and corrupt tag lineage.
            //
            // We therefore create:
            //   call_bb -> ret_take_bb -> ret_take_cont_bb -> orig_target
            // and only rewrite this call's `target` to `ret_take_bb`.
            if let InstrKind::RetTake { callee_id, dst_local } = creation_kind {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing tag local for RetTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, call_source, fn_span, .. } => {
                            let tgt = target.expect("call without target for RetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                // Compute the address from the return place. For wide pointers we extract the
                // data pointer first so the tag maps to the same address used by raw reads.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(dst_local),
                        addr_local,
                    )
                    .expect("RetTake on non-pointer local");

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_ret_tag_or_root,
                    std::iter::empty(),
                    source_info.span,
                );

                let dst_ty = body.local_decls[dst_local].ty;
                let is_mut = match dst_ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                        // Call-return ref retagging can lose the caller-side parent tag and
                        // materialize a fresh root at the same stack address. Mark these for
                        // runtime same-address lineage repair.
                        flags |= 0b10;
                    }
                    flags
                };
                let bounds_len_op = if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, dst_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                    tcx,
                    body,
                    source_info,
                    &bounds_len_op,
                );
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 }),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            alias_flags,
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: orig_target },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        take_bd.statements.push(addr_stmt1);
                    }
                    take_bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        take_bd.statements.append(&mut bounds_len_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                // Redirect only this call edge to the RetTake trampoline block.
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                }

                // Keep ref-ancestor in sync for return-tag recovery.
                // Without this, later PtrDerive on the returned pointer may pick an
                // uninitialized ref-ancestor local (0) and lose provenance, causing
                // OOB accesses to degrade into WILD_POINTER.
                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }

                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::RetPush { callee_id, ptr_local } = creation_kind {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RetPush");

                // Use the data pointer for wide return values so tag passing stays consistent.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                // Use the same address extraction helper for wide pointers.
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("RetPush on non-pointer local");

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let bd = &mut body.basic_blocks_mut()[bb];
                // Put the address computation in the current block, then call push, then jump to old Return.
                if let Some(addr_stmt1) = addr_stmt1_opt {
                    bd.statements.push(addr_stmt1);
                }
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagProp {
                dst,
                src,
                copy_tag,
                copy_ref_ancestor,
            } = creation_kind
            {
                // println!(
                //     "[instrument-mir] TAG PROPAGATION: dst_local={:?} src_local={:?}",
                //     dst,
                //     src
                // );
                let prop_stmt = if copy_tag {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for TagProp dst");

                    let src_op: Operand<'tcx> = if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*src_tag))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_tag),
                            Rvalue::Use(src_op),
                        ))),
                    ))
                } else {
                    None
                };

                let prop_ref_ancestor_stmt = if copy_ref_ancestor {
                    let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for TagProp dst");
                    let src_ref_ancestor_op: Operand<'tcx> =
                        if let Some(src_ref_ancestor) = ref_ancestor_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*src_ref_ancestor))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_ref_ancestor),
                            Rvalue::Use(src_ref_ancestor_op),
                        ))),
                    ))
                } else {
                    None
                };

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut next_insert = insert_at;
                if let Some(prop_stmt) = prop_stmt {
                    bd.statements.insert(next_insert, prop_stmt);
                    next_insert += 1;
                }
                if let Some(prop_ref_ancestor_stmt) = prop_ref_ancestor_stmt {
                    bd.statements.insert(next_insert, prop_ref_ancestor_stmt);
                }
                continue;
            }

            if let InstrKind::ArgRetag {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for ArgRetag");

                // Take the caller-pushed tag first, then create a fresh tag for this argument.
                let (record_def_id, is_mut_u8) = match body.local_decls[ptr_local].ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_ref, if is_mut { 1 } else { 0 })
                    }
                    TyKind::RawPtr(_ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_raw, if is_mut { 1 } else { 0 })
                    }
                    _ => panic!("ArgRetag on non-pointer local"),
                };
                let alias_exempt = self.alias_exempt_for_ptr_ty(
                    tcx,
                    body,
                    body.local_decls[ptr_local].ty,
                );
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        // Argument retagging often introduces short-lived receiver borrows at
                        // call boundaries. If source-tag plumbing drops the parent, allow runtime
                        // same-address repair instead of creating a sibling root.
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let bounds_len_op = self.bounds_len_operand_for_ptr_local(
                    tcx,
                    body,
                    ptr_local,
                    source_info.span,
                );
                let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                    tcx,
                    body,
                    source_info,
                    &bounds_len_op,
                );

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned { node: arg_callee, span: source_info.span },
                    Spanned { node: arg_index, span: source_info.span },
                    Spanned { node: arg_addr, span: source_info.span },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned { node: Operand::Copy(Place::from(addr_local)), span: source_info.span },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            alias_flags,
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };

                let record_func = Operand::function_handle(
                    tcx,
                    record_def_id,
                    std::iter::empty(),
                    source_info.span,
                );
                let record_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: record_func,
                        args: args_record,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let retag_block = {
                    let retag_data = BasicBlockData::new(Some(record_term), is_cleanup);
                    body.basic_blocks_mut().push(retag_data)
                };

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_call_arg_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(parent_tag_local),
                        target: Some(retag_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::FnExit { callee_id } = creation_kind {
                let exit_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_exit_fn,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_exit: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: self.const_u64(tcx, source_info.span, callee_id),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: exit_func,
                        args: args_exit,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            let func_operand = self.func_operand_for(tcx, hooks, &creation_kind, source_info.span);

            let insert_before: bool = ip.insert_before;

            let tag_local: Option<Local> = match creation_kind {
                InstrKind::Ref { .. } | InstrKind::Raw { .. } => {
                    if let Some(lhs_local) = place.as_local() {
                        Some(
                            *tag_local_for_ptr_local
                                .get(&lhs_local)
                                .expect("missing preallocated tag local for pointer destination"),
                        )
                    } else {
                        None
                    }
                }
                InstrKind::RetRoot { dst_local, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst_local)
                        .expect("missing preallocated tag local for RetRoot destination"),
                ),
                InstrKind::PtrDerive { dst, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing preallocated tag local for PtrDerive destination"),
                ),
                _ => None,
            };

            let addr_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));

            // Compute the address for runtime hooks. We route all pointer cases through
            // addr_stmts_for_place so wide pointers are handled via their data pointer.
            let mut addr_extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            let (addr_stmt1_opt, addr_stmt2) = match creation_kind {
                InstrKind::StackAlloc { local, .. } => {
                    let local_ty = body.local_decls[local].ty;
                    let ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
                    if !self.is_addr_exposable_ptr_ty(tcx, body, ptr_ty) {
                        continue;
                    }
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(ptr_ty, source_info.span));

                    let s1 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::RawPtr(RawPtrKind::Const, Place::from(local)),
                        ))),
                    );

                    let s2 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(Place::from(tmp_ptr)),
                                tcx.types.usize,
                            ),
                        ))),
                    );

                    (Some(s1), s2)
                }
                // Heap/const allocations already have a pointer local; just expose its address.
                InstrKind::HeapAlloc { ptr_local, .. } | InstrKind::ConstAlloc { ptr_local, .. } => {
                    match self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    ) {
                        Some(stmts) => stmts,
                        None => continue,
                    }
                }
                InstrKind::ConstAllocConst { const_op, .. } => {
                    let const_ty = const_op.const_.ty();
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(const_ty, source_info.span));
                    let assign_const = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::Use(Operand::Constant(Box::new(const_op.clone()))),
                        ))),
                    );
                    let Some((opt_stmt, addr_stmt)) = self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(tmp_ptr),
                        addr_local,
                    ) else {
                        continue;
                    };
                    // If we need multiple address statements (wide pointer), skip.
                    if opt_stmt.is_some() {
                        continue;
                    }
                    (Some(assign_const), addr_stmt)
                }
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. } => {
                    if let Some((opt, stmt, offset_stmts)) =
                        self.addr_stmts_for_access_place(tcx, body, source_info, place, addr_local)
                    {
                        addr_extra_stmts = offset_stmts;
                        (opt, stmt)
                    } else {
                        // Fallback for projection-heavy deref accesses where static offset
                        // recovery failed (e.g., generic field layout):
                        // materialize `&raw const <full place>` and expose that pointer.
                        // Using only `place.local` here points at the base carrier and can
                        // turn valid projected accesses into false OOB/WILD reports.
                        let place_ty = place.ty(&body.local_decls, tcx).ty;
                        let raw_ptr_ty = Ty::new_imm_ptr(tcx, place_ty);
                        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
                            continue;
                        }
                        let tmp_ptr = body
                            .local_decls
                            .push(LocalDecl::new(raw_ptr_ty, source_info.span));
                        let raw_ptr_stmt = Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tmp_ptr),
                                Rvalue::RawPtr(RawPtrKind::Const, place),
                            ))),
                        );
                        match self.addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(tmp_ptr),
                            addr_local,
                        ) {
                            Some((opt_stmt, addr_stmt)) => {
                                if let Some(s) = opt_stmt {
                                    addr_extra_stmts.push(s);
                                }
                                (Some(raw_ptr_stmt), addr_stmt)
                            }
                            None => continue,
                        }
                    }
                }
                _ => {
                    match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                        Some(stmts) => stmts,
                        None => continue,
                    }
                }
            };

            let (orig_term, is_cleanup) = {
                let bd = &mut body.basic_blocks_mut()[bb];
                let term = bd.terminator.take();
                let cleanup = bd.is_cleanup;
                (term, cleanup)
            };

            let cont_block = {
                let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                body.basic_blocks_mut().push(cont_data)
            };

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc { ptr_local, live, .. } => Some((*ptr_local, *live)),
                _ => None,
            };

            // For const/global allocations, rewrite the exposed pointer address to the base.
            let mut arg_addr_local = addr_local;
            let mut addr_adjust_stmt_opt: Option<Statement<'tcx>> = None;

            if let InstrKind::ConstAlloc { base_offset, .. }
            | InstrKind::ConstAllocConst { base_offset, .. } = &creation_kind
            {
                if *base_offset != 0 {
                    let base_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let offset_op = self.const_usize(tcx, source_info.span, *base_offset);
                    addr_adjust_stmt_opt = Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(base_local),
                            Rvalue::BinaryOp(
                                BinOp::Sub,
                                Box::new((Operand::Copy(Place::from(addr_local)), offset_op)),
                            ),
                        ))),
                    ));
                    arg_addr_local = base_local;
                }
            }

            let arg_addr = Operand::Copy(Place::from(arg_addr_local));

            let mut extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            if !addr_extra_stmts.is_empty() {
                extra_stmts.extend(addr_extra_stmts);
            }

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead { ptr_local, ref size_op }
                | InstrKind::PtrReadAllowUntagged { ptr_local, ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let (arg_size, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        size_op,
                    );
                    extra_stmts.append(&mut size_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackAlloc { ref size_op, live, .. } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        size_op,
                    );
                    extra_stmts.append(&mut size_stmts);
                    // Encode stack-alloc flag in bit1; bit0 is the live flag.
                    let live_bits: u8 = if live { 1 } else { 0 };
                    let arg_live = self.const_u8(tcx, source_info.span, live_bits | 0x2);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                        Spanned { node: arg_live, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::HeapAlloc { ptr_local: _, live, ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        size_op,
                    );
                    extra_stmts.append(&mut size_stmts);
                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                        Spanned { node: arg_live, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                // Record a live global/promoted allocation at the computed base address.
                InstrKind::ConstAlloc { size, .. } | InstrKind::ConstAllocConst { size, .. } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let arg_size = self.const_usize(tcx, source_info.span, size);
                    let arg_live = self.const_u8(tcx, source_info.span, 1);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                        Spanned { node: arg_live, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrWrite { ptr_local, ref size_op }
                | InstrKind::PtrWriteAllowUntagged { ptr_local, ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let (arg_size, mut size_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        size_op,
                    );
                    extra_stmts.append(&mut size_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgPush {
                    callee_id,
                    arg_index,
                    ptr_local,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // For projected call arguments such as subslices (`output[a..b]`) or
                        // field projections, pushing the carrier local's current tag is often too
                        // weak: optimized MIR can keep only a wrapper temp tagged while the actual
                        // projected argument never materializes its own stable tag before the call.
                        //
                        // The callee only needs a parent lineage to retag its local argument at
                        // the callee address. Reuse the same parent-selection logic we use for
                        // ref/raw creation so interprocedural retagging stays attached to the
                        // source borrow family instead of falling back to a root inside the callee.
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            false,
                        )
                    };

                    let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                    let arg_index = self.const_u64(tcx, source_info.span, arg_index);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_callee, span: source_info.span },
                        Spanned { node: arg_index, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: tag_op, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrUse { ptr_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrDerive { dst, src, is_mut, is_ref } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDerive dst");

                    let parent_from_src: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*tl))
                    } else if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let parent_tag_op: Operand<'tcx> = match &creation_kind {
                        // For derivations, use the source local's current tag when available.
                        // Falling back to ref-ancestor is only a backup path.
                        InstrKind::PtrDerive { is_ref: true, .. } => parent_from_src,
                        _ => {
                            // Keep raw derivations connected to source provenance.
                            parent_from_src
                        }
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    // Bitfield semantics match __record_* hooks:
                    // bit0=alias_exempt, bit1=basic lineage-repair hint, bit2=strong hint,
                    // bit3=carry wide bounds from src when derivation drops metadata.
                    // Raw PtrDerive in optimized MIR often comes from projection-heavy lowering
                    // and benefits from runtime parent repair when stack metadata is coarse.
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if self.should_forward_bounds_from_src_ptr_derive(tcx, body, src, dst) {
                            alias_flags |= 0b1000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op = self.bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        &bounds_len_op,
                    );
                    extra_stmts.append(&mut bounds_len_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_mut, span: source_info.span },
                        Spanned { node: parent_tag_op, span: source_info.span },
                        Spanned { node: arg_alias, span: source_info.span },
                        Spanned { node: arg_bounds_len, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                _ => {
                    let is_mut_u8: u8 = match creation_kind {
                        InstrKind::Ref { bk: borrow_kind, .. } => match borrow_kind {
                            BorrowKind::Mut { .. } => 1,
                            _ => 0,
                        },
                        InstrKind::Raw { is_mut, .. } => if is_mut { 1 } else { 0 },
                        InstrKind::RetRoot { is_mut, .. } => if is_mut { 1 } else { 0 },
                        _ => 0,
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, is_mut_u8);

                    let arg_parent: Operand<'tcx> = match &creation_kind {
                        InstrKind::Ref { src, .. } => {
                            self.parent_tag_operand_for_src_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                source_info,
                                *src,
                                tag_local_for_ptr_local,
                                ref_ancestor_local_for_ptr_local,
                                false,
                            )
                        }
                        InstrKind::Raw { src, .. } => {
                            let src_local = src.local;
                            let src_is_plain_ref_deref = src.projection.len() == 1
                                && matches!(src.projection[0], ProjectionElem::Deref)
                                && matches!(body.local_decls[src_local].ty.kind(), TyKind::Ref(..));
                            if src_is_plain_ref_deref {
                                if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                                    Operand::Copy(Place::from(*tl))
                                } else if let Some(tl) =
                                    ref_ancestor_local_for_ptr_local.get(&src_local)
                                {
                                    Operand::Copy(Place::from(*tl))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }
                            } else {
                            self.parent_tag_operand_for_src_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                source_info,
                                *src,
                                tag_local_for_ptr_local,
                                ref_ancestor_local_for_ptr_local,
                                true,
                            )
                            }
                        }
                        _ => self.const_u64(tcx, source_info.span, 0),
                    };

                    let alias_exempt = match &creation_kind {
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            let ty = src.ty(&body.local_decls, tcx).ty;
                            self.alias_exempt_for_ty(tcx, body, ty)
                        }
                        InstrKind::RawRoot { ptr_local, .. } => {
                            let ty = body.local_decls[*ptr_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let ty = body.local_decls[*dst_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        _ => false,
                    };
                    // `alias_exempt` argument is a bitfield:
                    // - bit0: alias-exempt pointee classification (existing behavior)
                    // - bit1: projected-source creation hint (used by runtime lineage repair)
                    // - bit2: stronger root-origin repair hint (bounded overlap recovery)
                    let alias_flags: u8 = match &creation_kind {
                        InstrKind::Ref { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Even simple reference reborrows can lose their parent tag in
                            // optimized MIR/call-boundary lowering and show up as fresh roots at
                            // the same stack address. Always allow exact same-address repair for
                            // refs; keep the stronger bounded-overlap recovery limited to
                            // projection-heavy sources.
                            flags |= 0b10;
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                            }
                            let src_ty = src.ty(&body.local_decls, tcx).ty;
                            if self.is_pointer_ty(src_ty) && !self.is_thin_ptr_ty(tcx, body, src_ty) {
                                // Wide-pointer reborrows often lower through a temporary thin raw
                                // data pointer. If that helper raw root loses lineage, the runtime
                                // should prefer dropping the bad raw-root parent over freezing the
                                // eventual wide ref/write as a foreign sibling.
                                flags |= 0b1_0000;
                            }
                            flags
                        }
                        InstrKind::Raw { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Raw creation can still lose lineage when source-tag plumbing is
                            // missing (e.g. wrapper/projection-heavy optimized MIR). Mark all
                            // raw creations as eligible for runtime best-effort repair.
                            flags |= 0b10;
                            // For projected raw sources (`(*p).field`, etc.) also allow strong
                            // bounded-overlap parent recovery in the runtime repair path.
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                            }
                            flags
                        }
                        // RawRoot is emitted exactly in cases where provenance source recovery
                        // failed at instrumentation time. Mark for runtime best-effort repair.
                        InstrKind::RawRoot { .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            flags |= 0b10;
                            flags
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Return-root creation means caller-side provenance recovery failed.
                            // Mark both refs and raws as eligible for exact same-address repair:
                            // uninstrumented std/core pointer-returning wrappers can otherwise
                            // synthesize a fresh root for a pointer that should remain attached to
                            // an existing live lineage at the same address.
                            flags |= 0b10;
                            flags
                        }
                        _ => {
                            if alias_exempt { 1 } else { 0 }
                        }
                    };
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);

                let bounds_ptr_local = match &creation_kind {
                        InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                        InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                        InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                        _ => None,
                    };
                    let bounds_len_op = bounds_ptr_local
                        .map(|pl| match &creation_kind {
                            InstrKind::Ref { .. } => {
                                self.ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                )
                            }
                            InstrKind::RetRoot { .. }
                                if matches!(body.local_decls[pl].ty.kind(), TyKind::Ref(..)) =>
                            {
                                self.ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                )
                            }
                            _ => self.bounds_len_operand_for_ptr_local(
                                tcx,
                                body,
                                pl,
                                source_info.span,
                            ),
                        })
                        .unwrap_or_else(|| {
                            SizeOperand::Const(self.const_usize(tcx, source_info.span, 0))
                        });
                    let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                        tcx,
                        body,
                        source_info,
                        &bounds_len_op,
                    );
                    extra_stmts.append(&mut bounds_len_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_mut, span: source_info.span },
                        Spanned { node: arg_parent, span: source_info.span },
                        Spanned { node: arg_alias, span: source_info.span },
                        Spanned { node: arg_bounds_len, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    let dst = tag_local.expect("missing tag_local for ref/raw creation");
                    (args, Place::from(dst))
                }
            };

            let mut call_term = Terminator {
                source_info,
                kind: TerminatorKind::Call {
                    func: func_operand,
                    args,
                    destination: dest_place,
                    target: Some(cont_block),
                    unwind: UnwindAction::Continue,
                    call_source: CallSource::Misc,
                    fn_span: source_info.span,
                },
            };

            if let Some((ptr_local, true)) = heap_alloc_info {
                let raw_bb = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));

                if let TerminatorKind::Call { target, .. } = &mut call_term.kind {
                    *target = Some(raw_bb);
                }

                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for heap alloc ptr");

                let raw_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_raw,
                    std::iter::empty(),
                    source_info.span,
                );
                let alias_exempt = self.alias_exempt_for_ptr_ty(
                    tcx,
                    body,
                    body.local_decls[ptr_local].ty,
                );

                let raw_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, 1),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            if alias_exempt { 1 } else { 0 },
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[raw_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: raw_func,
                        args: raw_args,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
            }

            let ref_ancestor_init_stmt_opt: Option<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref { .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let (Some(dst_ref_ancestor_local), Some(dst_tag_local)) = (
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied(),
                            tag_local_for_ptr_local.get(&dst_local).copied(),
                        ) {
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(Operand::Copy(Place::from(dst_tag_local))),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(&src.local)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::RawRoot { ptr_local, .. } => {
                    ref_ancestor_local_for_ptr_local
                        .get(ptr_local)
                        .copied()
                        .map(|dst_ref_ancestor_local| {
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                                ))),
                            )
                        })
                }
                InstrKind::RetRoot {
                    dst_local, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst_local).copied()
                    {
                        if *is_ref {
                            tag_local_for_ptr_local.get(dst_local).copied().map(|dst_tag_local| {
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(dst_ref_ancestor_local),
                                        Rvalue::Use(Operand::Copy(Place::from(dst_tag_local))),
                                    ))),
                                )
                            })
                        } else {
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                InstrKind::PtrDerive {
                    dst, src, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst).copied()
                    {
                        if *is_ref {
                            tag_local_for_ptr_local.get(dst).copied().map(|dst_tag_local| {
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(dst_ref_ancestor_local),
                                        Rvalue::Use(Operand::Copy(Place::from(dst_tag_local))),
                                    ))),
                                )
                            })
                        } else {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(src)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };

            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

                let len = bd.statements.len();
                let split_at = if stmt_idx >= len {
                    len
                } else if insert_before {
                    stmt_idx
                } else {
                    stmt_idx + 1
                };

                let rem = bd.statements.split_off(split_at);

                if let Some(s1) = addr_stmt1_opt {
                    bd.statements.push(s1);
                }
                bd.statements.push(addr_stmt2);
                if let Some(s3) = addr_adjust_stmt_opt {
                    bd.statements.push(s3);
                }
                if !extra_stmts.is_empty() {
                    bd.statements.extend(extra_stmts);
                }

                bd.terminator = Some(call_term);
                rem
            };

            if let Some(ref_ancestor_init_stmt) = ref_ancestor_init_stmt_opt {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .insert(0, ref_ancestor_init_stmt);
            }
            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
        }

        // Insert ArgRetag points after all other instrumentation.
        // Because each insertion rewrites the entry terminator, the last-applied
        // retag runs first at runtime, ensuring tags are initialized before reads.
        for (_idx, ip) in arg_retag_points.into_iter().rev() {
            let bb = ip.bb;
            let stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            if let InstrKind::ArgRetag {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for ArgRetag");

                // Take the caller-pushed tag first, then create a fresh tag for this argument.
                let (record_def_id, is_mut_u8) = match body.local_decls[ptr_local].ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_ref, if is_mut { 1 } else { 0 })
                    }
                    TyKind::RawPtr(_ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_raw, if is_mut { 1 } else { 0 })
                    }
                    _ => panic!("ArgRetag on non-pointer local"),
                };
                let alias_exempt = self.alias_exempt_for_ptr_ty(
                    tcx,
                    body,
                    body.local_decls[ptr_local].ty,
                );
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let ptr_ty = body.local_decls[ptr_local].ty;
                let bounds_len_op = if matches!(ptr_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        ptr_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) = self.materialize_size_operand(
                    tcx,
                    body,
                    source_info,
                    &bounds_len_op,
                );

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned { node: arg_callee, span: source_info.span },
                    Spanned { node: arg_index, span: source_info.span },
                    Spanned { node: arg_addr, span: source_info.span },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned { node: Operand::Copy(Place::from(addr_local)), span: source_info.span },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };

                let record_func = Operand::function_handle(
                    tcx,
                    record_def_id,
                    std::iter::empty(),
                    source_info.span,
                );
                let record_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: record_func,
                        args: args_record,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let retag_block = {
                    let retag_data = BasicBlockData::new(Some(record_term), is_cleanup);
                    body.basic_blocks_mut().push(retag_data)
                };

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_call_arg_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(parent_tag_local),
                        target: Some(retag_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
            }
        }
    }

    fn find_def_id_by_name<'tcx>(&self, tcx: TyCtxt<'tcx>, target_name: &str) -> Option<DefId> {
        let debug = std::env::var("RZ_DEBUG_SYMBOL_LOOKUP")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                if debug {
                    println!("Searching for '{}' in runtime crate:", target_name);
                }
                let items = tcx.exported_non_generic_symbols(cnum);
                if debug {
                    println!("Found {} items", items.len());
                }
                for (symbol, _) in items {
                    match symbol {
                        ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                            if let Some(name) = tcx.opt_item_name(*def_id) {
                                if debug {
                                    println!(" - Checking item: {}", name);
                                }
                                if name.as_str() == target_name {
                                    if debug {
                                        println!(" - Match found for '{}'", target_name);
                                    }
                                    return Some(*def_id);
                                }
                            } else {
                                if debug {
                                    println!(" - Unnamed item: {:?}", def_id);
                                }
                            }
                        }
                        ExportedSymbol::NoDefId(symbol_name) => {
                            if debug {
                                println!(" - Symbol without DefId: {:?}", symbol_name);
                            }
                        }
                        ExportedSymbol::DropGlue(ty) => {
                            if debug {
                                println!(" - DropGlue for type: {:?}", ty);
                            }
                        }
                        ExportedSymbol::AsyncDropGlueCtorShim(ty) => {
                            if debug {
                                println!(" - AsyncDropGlueCtorShim for type: {:?}", ty);
                            }
                        }
                        ExportedSymbol::AsyncDropGlue(def_id, ty) => {
                            if debug {
                                println!(" - AsyncDropGlue for DefId: {:?}, type: {:?}", def_id, ty);
                            }
                        }
                        ExportedSymbol::ThreadLocalShim(def_id) => {
                            if debug {
                                println!(" - ThreadLocalShim for DefId: {:?}", def_id);
                            }
                        }
                        _ => {
                            if debug {
                                println!(" - Unhandled ExportedSymbol variant");
                            }
                        }
                    }
                }
            }
        }
        println!("No match found for '{}'", target_name);
        None
    }

    fn find_runtime_fn_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        target_name: &str,
        expected_inputs: usize,
    ) -> Option<DefId> {
        let debug = std::env::var("RZ_DEBUG_SYMBOL_LOOKUP")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");
        let mut fallback_name_match: Option<DefId> = None;

        for &cnum in tcx.crates(()).iter() {
            if tcx.crate_name(cnum).as_str() != "runtime" {
                continue;
            }
            let items = tcx.exported_non_generic_symbols(cnum);
            for (symbol, _) in items {
                let def_id = match symbol {
                    ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => *def_id,
                    _ => continue,
                };
                let Some(name) = tcx.opt_item_name(def_id) else {
                    continue;
                };
                if name.as_str() != target_name {
                    continue;
                }
                fallback_name_match.get_or_insert(def_id);

                let sig = tcx.fn_sig(def_id).skip_binder();
                let inputs = sig.inputs().skip_binder().len();
                if debug {
                    println!(
                        "candidate runtime hook {} => {:?} {}({} args)",
                        target_name,
                        def_id,
                        tcx.def_path_str(def_id),
                        inputs
                    );
                }
                if inputs == expected_inputs {
                    return Some(def_id);
                }
            }
        }

        fallback_name_match
    }

    pub(crate) fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let trace_pass = std::env::var("RZ_TRACE_PASS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");
        let def_id = body.source.def_id();
        let def_path = tcx.def_path_str(def_id);

        if def_path.contains("runtime") {
            if trace_pass {
                println!("Skipping optimization for {}", def_path);
            }
            return;
        }

        let crate_name = tcx.crate_name(def_id.krate);

        // Skip the `runtime` crate
        if crate_name.as_str() == "runtime" {
            if trace_pass {
                println!(
                    "Skipping optimization for item in runtime crate: {:?}",
                    def_id
                );
            }
            return;
        }

        if body.coroutine.is_some() {
            // Async lowering creates coroutine state machines. Injecting locals or
            // control-flow edits in those bodies can trigger rustc recursion/cycle
            // errors (observed with Tokio). Skip coroutine bodies for now.
            if trace_pass {
                println!("Skipping optimization for coroutine body: {:?}", def_id);
            }
            return;
        }

        // Skip build scripts to avoid ICEs in codegen (e.g. wide ptr operands in build.rs).
        if crate_name.as_str() == "build_script_build" || def_path.contains("build_script_build") {
            if trace_pass {
                println!("Skipping optimization for build script: {:?}", def_id);
            }
            return;
        }

        // Skip proc-macro crates: they execute on the host during compilation
        // (derive/attribute expansion). Instrumenting them causes compile-time
        // runtime reports that are unrelated to the fuzz target itself.
        if tcx
            .sess
            .opts
            .crate_types
            .iter()
            .any(|ct| matches!(ct, rustc_session::config::CrateType::ProcMacro))
        {
            if trace_pass {
                println!("Skipping optimization for proc-macro crate: {:?}", def_id);
            }
            return;
        }

        if trace_pass {
            println!(
                "Running MyOptimizationPass on {:?} {:?}",
                body.source.def_id(),
                def_path
            );
        }

        // Some helpers are intentionally classified as pure scalar queries. Instrumenting their
        // bodies only materializes administrative borrows that can outlive the query and create
        // false Tree Borrows freezes in callers. `fdeflate::Decompressor::is_done` is the current
        // concrete case in the PNG decode path.
        if def_path.contains("decompress::Decompressor") && def_path.ends_with("::is_done") {
            return;
        }

        if self.analyze_unsafe_summaries_only_enabled() {
            let unsafe_influence = unsafe_dataflow::compute_unsafe_influence(
                tcx,
                body,
                self.unsafe_dataflow_selective_enabled(),
            );
            self.log_unsafe_dataflow_call_stats(tcx, body, &unsafe_influence);
            self.log_unsafe_dataflow_summary_stats(tcx, body, &unsafe_influence);
            self.dump_unsafe_dataflow_summary(tcx, body, &unsafe_influence);
            return;
        }


        // self.print_runtime_items(tcx);

        let def_id_ref = self
            .find_runtime_fn_def_id(tcx, "__record_ref_creation", 5)
            .expect("missing '__record_ref_creation' definition");
        let def_id_raw = self
            .find_runtime_fn_def_id(tcx, "__record_raw_ptr_creation", 5)
            .expect("missing '__record_raw_ptr_creation' definition");
        let def_id_alloc = self
            .find_runtime_fn_def_id(tcx, "__rz_record_alloc", 3)
            .expect("missing '__rz_record_alloc' definition");
        let def_id_write = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write", 3)
            .expect("missing '__rz_ptr_write' definition");
        let def_id_write_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write_allow_untagged", 3)
            .expect("missing '__rz_ptr_write_allow_untagged' definition");
        let def_id_read = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read", 3)
            .expect("missing '__rz_ptr_read' definition");
        let def_id_read_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read_allow_untagged", 3)
            .expect("missing '__rz_ptr_read_allow_untagged' definition");
        let def_id_use = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_use", 2)
            .expect("missing '__rz_ptr_use' definition");
        let def_id_push_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_call_arg_tag", 4)
            .expect("missing '__rz_push_call_arg_tag' definition");
        let def_id_take_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_tag", 3)
            .expect("missing '__rz_take_call_arg_tag' definition");
        let def_id_push_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_ret_tag", 3)
            .expect("missing '__rz_push_ret_tag' definition");
        let def_id_take_ret_tag_or_root = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_tag_or_root", 5)
            .expect("missing '__rz_take_ret_tag_or_root' definition");
        let def_id_exit_fn = self
            .find_runtime_fn_def_id(tcx, "__rz_exit_fn", 1)
            .expect("missing '__rz_exit_fn' definition");

        let hooks = Hooks {
            def_id_ref,
            def_id_raw,
            def_id_alloc,
            def_id_write,
            def_id_write_allow_untagged,
            def_id_read,
            def_id_read_allow_untagged,
            def_id_use,
            def_id_push_call_arg_tag,
            def_id_take_call_arg_tag,
            def_id_push_ret_tag,
            def_id_take_ret_tag_or_root,
            def_id_exit_fn,
        };

        let unsafe_influence = unsafe_dataflow::compute_unsafe_influence(
            tcx,
            body,
            self.unsafe_dataflow_selective_enabled(),
        );
        self.log_unsafe_dataflow_summary_stats(tcx, body, &unsafe_influence);
        self.dump_unsafe_dataflow_summary(tcx, body, &unsafe_influence);
        if self.trace_unsafe_dataflow_enabled() && unsafe_influence.enabled() {
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-dflow] fn={} tainted_ptrs={} total_ptrs={}",
                def_path,
                unsafe_influence.tainted_ptr_count(),
                unsafe_influence.total_ptr_count()
            );
        }

        let scan = self.scan_body(tcx, body, &unsafe_influence);
        let tag_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        // Per-pointer local "nearest reference ancestor" tag.
        // This is used as the preferred parent for raw/derived pointer creations so
        // provenance survives wrapper-heavy flows (casts, calls, ret-take, etc.).
        //
        // Example:
        //   let r: &u32 = &x;            // ref tag R
        //   let p1: *const u32 = r as *const u32;   // raw tag P1
        //   let p2 = unsafe { p1.add(1) };          // raw tag P2
        // For P2 we want parent lineage to stay anchored to R (nearest ref ancestor),
        // not collapse to an uninitialized/zero parent through raw-only hops.
        // Keep this map synchronized with `tag_local_for_ptr_local` initialization paths.
        let ref_ancestor_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag);

        let mut insert_points = scan.insert_points;
        // Box::from_raw rewraps an existing allocation. Suppress any dead HeapAlloc
        // hook placed at its call site so drop can read the pointee safely.
        insert_points.retain(|ip| {
            if let InstrKind::HeapAlloc { ptr_local, live: false, .. } = ip.kind {
                if let Some(term) = body.basic_blocks[ip.bb].terminator.as_ref() {
                    if let TerminatorKind::Call { args, destination, .. } = &term.kind {
                        let arg0_local = args
                            .get(0)
                            .and_then(|arg| self.place_from_operand(&arg.node))
                            .map(|p| p.local);
                        if arg0_local == Some(ptr_local) {
                            if let Some(dst_local) = destination.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                if let TyKind::Adt(adt, _) = dst_ty.kind() {
                                    let name = tcx.def_path_str(adt.did());
                                    if name.contains("boxed::Box") || name.contains("::boxed::Box") {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            true
        });

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            &tag_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            hooks,
        );

        // Avoid reading uninitialized tag locals in callees: default them to 0 ("untagged").
        // IMPORTANT: do this AFTER insert_instrumentation so we don't invalidate `stmt_idx`
        // computed by scan_body for START_BLOCK (bb0).
        let mut arg_ptr_locals: HashSet<Local> = HashSet::new();
        for arg_local in body.args_iter() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_pointer_ty(arg_ty) {
                arg_ptr_locals.insert(arg_local);
            }
        }

        self.init_tag_locals_to_zero(tcx, body, &tag_local_for_ptr_local, &arg_ptr_locals);
        self.init_tag_locals_to_zero(
            tcx,
            body,
            &ref_ancestor_local_for_ptr_local,
            &arg_ptr_locals,
        );

        // Defensive fixup: instrumentation should always leave valid terminators, but avoid
        // crashing rustc if a block ends up missing one in complex crates.
        let mut missing_terminators: Vec<BasicBlock> = Vec::new();
        let body_span = body.span;
        for (bb, bd) in body.basic_blocks_mut().iter_enumerated_mut() {
            if bd.terminator.is_none() {
                missing_terminators.push(bb);
                bd.terminator = Some(Terminator {
                    source_info: SourceInfo {
                        span: body_span,
                        scope: OUTERMOST_SOURCE_SCOPE,
                    },
                    kind: TerminatorKind::Unreachable,
                });
            }
        }
        if !missing_terminators.is_empty() {
            let mut pred_map: HashMap<BasicBlock, Vec<BasicBlock>> = HashMap::new();
            for (pred_bb, pred_bd) in body.basic_blocks.iter_enumerated() {
                if let Some(pred_term) = pred_bd.terminator.as_ref() {
                    for succ_bb in pred_term.kind.successors() {
                        pred_map.entry(succ_bb).or_default().push(pred_bb);
                    }
                }
            }
            rz_pass_warn!(
                self,
                "[rusteze][warn] inserted {} Unreachable terminators to repair malformed MIR in {}",
                missing_terminators.len(),
                def_path
            );
            for bb in missing_terminators.iter().take(16) {
                let preds = pred_map.get(bb).cloned().unwrap_or_default();
                let stmt_preview = body.basic_blocks[*bb]
                    .statements
                    .iter()
                    .take(3)
                    .map(|s| format!("{:?}", s.kind))
                    .collect::<Vec<_>>()
                    .join(" | ");
                rz_pass_warn!(
                    self,
                    "[rusteze][warn] malformed bb{}: cleanup={} pred_count={} preds={:?} stmt_count={} stmt_preview={}",
                    bb.index(),
                    body.basic_blocks[*bb].is_cleanup,
                    preds.len(),
                    preds.iter().map(|p| p.index()).collect::<Vec<_>>(),
                    body.basic_blocks[*bb].statements.len(),
                    stmt_preview
                );
            }
            if missing_terminators.len() > 16 {
                rz_pass_warn!(
                    self,
                    "[rusteze][warn] malformed MIR diagnostics truncated: {} additional blocks",
                    missing_terminators.len() - 16
                );
            }
        }
    }
}

fn call_effect_label(effect: CallEffect) -> &'static str {
    match effect {
        CallEffect::Ignore => "Ignore",
        CallEffect::MemCopy => "MemCopy",
        CallEffect::MemSet => "MemSet",
        CallEffect::Load => "Load",
        CallEffect::Store => "Store",
        CallEffect::PtrDerive => "PtrDerive",
        CallEffect::BoxIntoRaw => "BoxIntoRaw",
        CallEffect::BoxFromRaw => "BoxFromRaw",
        CallEffect::AllocShim(AllocShimKind::Alloc) => "AllocShim(Alloc)",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed) => "AllocShim(AllocZeroed)",
        CallEffect::AllocShim(AllocShimKind::Dealloc) => "AllocShim(Dealloc)",
        CallEffect::AllocShim(AllocShimKind::Realloc) => "AllocShim(Realloc)",
        CallEffect::AllocShim(AllocShimKind::No) => "AllocShim(No)",
        CallEffect::Unknown => "Unknown",
    }
}

pub(crate) fn debug_classify_call_effect(def_path: &str) -> &'static str {
    let pass = MyOptimizationPass;
    let effect = pass
        .match_call_effect_rule(def_path)
        .unwrap_or(CallEffect::Unknown);
    call_effect_label(effect)
}

#[cfg(test)]
mod tests {
    use super::{CallEffect, MyOptimizationPass};

    fn effect_for(def_path: &str) -> CallEffect {
        MyOptimizationPass
            .match_call_effect_rule(def_path)
            .unwrap_or(CallEffect::Unknown)
    }

    #[test]
    fn classify_common_helpers() {
        assert_eq!(
            effect_for("core::slice::<impl [T]>::get"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::is_empty"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::len"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::slice::index::<impl core::ops::Index<I> for [T]>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl core::ops::IndexMut<I> for [T]>::index_mut"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl std::slice::SliceIndex<[u8]> for std::ops::Range<usize>>::index_mut"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<std::ops::Range<usize> as std::slice::SliceIndex<[u8]>>::index_mut"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::slice::index::<impl std::slice::SliceIndex<[u8]> for usize>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<usize as std::slice::SliceIndex<[u8]>>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::ops::RangeBounds::start_bound"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("alloc::vec::Vec::<T, A>::len"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::convert::AsRef::as_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<T as core::convert::AsRef<U>>::as_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::ptr::NonNull::<T>::as_ptr"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::ptr::null"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("std::io::Cursor::<T>::position"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::fmt::Formatter::<'a>::write_fmt"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("core::sync::atomic::AtomicUsize::load"),
            CallEffect::Load
        );
        assert_eq!(
            effect_for("core::sync::atomic::AtomicUsize::compare_exchange"),
            CallEffect::Store
        );
        assert_eq!(
            effect_for("core::intrinsics::arith_offset"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("core::intrinsics::read_via_copy"),
            CallEffect::Load
        );
        assert_eq!(
            effect_for("core::intrinsics::write_via_move"),
            CallEffect::Store
        );
        assert_eq!(
            effect_for("<alloc::vec::Vec<T, A> as core::ops::Index<I>>::index"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            MyOptimizationPass.classify_call_effect(
                "decompress::Decompressor::is_done"
            ),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("rkyv::place::Place::<u32>::write"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("rkyv::place::Place::<u32>::write_unchecked"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("rkyv::place::<impl Place<u32>>::write"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("rkyv::place::<impl Place<u32>>::write_unchecked"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("place::Place::<u32>::write"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("place::Place::<u32>::write_unchecked"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("place::<impl Place<u32>>::write"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("place::<impl Place<u32>>::write_unchecked"),
            CallEffect::Ignore
        );
    }
}
