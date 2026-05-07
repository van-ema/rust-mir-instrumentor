use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

mod config;
mod metadata_dataflow;

// (rest unchanged)
// NOTE: This pass intentionally avoids instrumenting std/core/alloc directly.
use crate::unsafe_dataflow::{self, UnsafeInfluence, UnsafeSummaryRecord};
use rustc_abi::{FieldIdx, VariantIdx};
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::intravisit::{self, Visitor as HirVisitor};
use rustc_hir::lang_items::LangItem;
use rustc_hir::{self as hir, Mutability};
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::{GlobalAlloc, Scalar};
use rustc_middle::mir::traversal;
use rustc_middle::mir::visit::{MutatingUseContext, NonUseContext, PlaceContext, Visitor};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::TyKind;
use rustc_middle::ty::{
    ConstKind as TyConstKind, GenericArgsRef, Instance, PseudoCanonicalInput, Ty, TyCtxt, TypingEnv,
};
use rustc_middle::ty::{TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor};
use rustc_span::{Span, source_map::Spanned};

pub(crate) struct MyOptimizationPass;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SsaAnchorSource {
    Tag,
    RefAncestor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SsaAnchorState {
    local: Local,
    deps: Vec<Local>,
    source: SsaAnchorSource,
}

type SsaAnchorMap = HashMap<String, SsaAnchorState>;
type ReborrowAnchorSpecMap = HashMap<String, Vec<Local>>;

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
    fn visit_place(&mut self, place: &Place<'tcx>, context: PlaceContext, location: Location) {
        let is_def = matches!(
            context,
            PlaceContext::MutatingUse(MutatingUseContext::Store)
                | PlaceContext::MutatingUse(MutatingUseContext::Deinit)
                | PlaceContext::MutatingUse(MutatingUseContext::SetDiscriminant)
                | PlaceContext::MutatingUse(MutatingUseContext::AsmOutput)
                | PlaceContext::MutatingUse(MutatingUseContext::Call)
                | PlaceContext::MutatingUse(MutatingUseContext::Yield)
        );
        if !is_def && !matches!(context, PlaceContext::NonUse(_)) {
            self.stats.entry(place.local).or_default().uses += 1;
        }
        self.super_place(place, context, location);
    }
}

struct LocalUseFinder {
    local: Local,
    skip_call_dest: Option<Location>,
    found: bool,
}

impl<'tcx> Visitor<'tcx> for LocalUseFinder {
    fn visit_place(&mut self, place: &Place<'tcx>, context: PlaceContext, location: Location) {
        if self.found || place.local != self.local {
            self.super_place(place, context, location);
            return;
        }
        if self.skip_call_dest == Some(location)
            && matches!(context, PlaceContext::MutatingUse(MutatingUseContext::Call))
        {
            self.super_place(place, context, location);
            return;
        }
        let is_def = matches!(
            context,
            PlaceContext::MutatingUse(MutatingUseContext::Store)
                | PlaceContext::MutatingUse(MutatingUseContext::Deinit)
                | PlaceContext::MutatingUse(MutatingUseContext::SetDiscriminant)
                | PlaceContext::MutatingUse(MutatingUseContext::AsmOutput)
                | PlaceContext::MutatingUse(MutatingUseContext::Call)
                | PlaceContext::MutatingUse(MutatingUseContext::Yield)
        );
        if !is_def && !matches!(context, PlaceContext::NonUse(_)) {
            self.found = true;
        }
        self.super_place(place, context, location);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(in crate::instrumentation) enum PassLogLevel {
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

struct HirRefBindingCollector<'tcx> {
    typeck: &'tcx rustc_middle::ty::TypeckResults<'tcx>,
    bindings: Vec<HirRefBinding<'tcx>>,
}

impl<'tcx> HirVisitor<'tcx> for HirRefBindingCollector<'tcx> {
    fn visit_stmt(&mut self, stmt: &'tcx hir::Stmt<'tcx>) {
        if let hir::StmtKind::Let(let_stmt) = stmt.kind {
            let_stmt.pat.walk_always(|pat| {
                if let hir::PatKind::Binding(_, _, ident, _) = pat.kind {
                    let ty = self.typeck.pat_ty(pat);
                    if matches!(ty.kind(), TyKind::Ref(..)) {
                        self.bindings.push(HirRefBinding {
                            name: ident.name,
                            span: pat.span,
                            ty,
                        });
                    }
                }
            });
        }
        intravisit::walk_stmt(self, stmt);
    }
}

fn span_contains(outer: Span, inner: Span) -> bool {
    outer.ctxt() == inner.ctxt() && outer.lo() <= inner.lo() && inner.hi() <= outer.hi()
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
    LoadUnaligned,
    StoreUnaligned,
    /// Pointer derivation wrappers that return a pointer derived from arg0 (fresh tag, parent linkage).
    PtrDerive,
    /// Pointer-returning helpers that create a raw root from an integer/exposed address.
    ExposedProvenanceRoot,
    /// Wrapper constructors that return a non-pointer carrier whose pointer leaves come from arg0.
    CarrierCopyArg0,
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
        Self {
            kind1: kind,
            needle1: needle,
            kind2: None,
            needle2: None,
            effect,
        }
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
    EffectRule::one(
        MatchKind::Contains,
        "__rust_alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "__rust_realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // alloc::alloc wrappers.
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::exchange_malloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "alloc::alloc::realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // std::alloc wrappers (often take `Layout`).
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::alloc_zeroed",
        CallEffect::AllocShim(AllocShimKind::AllocZeroed),
    ),
    // Keep this after alloc_zeroed so it doesn't catch it first.
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::alloc",
        CallEffect::AllocShim(AllocShimKind::Alloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::dealloc",
        CallEffect::AllocShim(AllocShimKind::Dealloc),
    ),
    EffectRule::one(
        MatchKind::Contains,
        "std::alloc::realloc",
        CallEffect::AllocShim(AllocShimKind::Realloc),
    ),
    // No-op helpers.
    EffectRule::one(MatchKind::EndsWith, "::is_null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::addr_eq", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::ptr::null_mut", CallEffect::Ignore),
    EffectRule::one(
        MatchKind::Contains,
        "::without_provenance",
        CallEffect::ExposedProvenanceRoot,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::with_exposed_provenance",
        CallEffect::ExposedProvenanceRoot,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::const_ptr",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::mut_ptr",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Ignore,
    ),
    // ---- Common std/core helpers (suppress unknown-call noise) ----

    // Deref/DerefMut return a reference derived from self.
    EffectRule::two(
        MatchKind::Contains,
        "::ops::deref::Deref",
        MatchKind::EndsWith,
        "::deref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::deref::DerefMut",
        MatchKind::EndsWith,
        "::deref_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Deref",
        MatchKind::EndsWith,
        "::deref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::DerefMut",
        MatchKind::EndsWith,
        "::deref_mut",
        CallEffect::PtrDerive,
    ),
    // Iterator metadata helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::iter::traits::iterator::Iterator",
        MatchKind::EndsWith,
        "::size_hint",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::iter::Iterator",
        MatchKind::EndsWith,
        "::size_hint",
        CallEffect::Ignore,
    ),
    // Slice helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::iter",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::iter_mut",
        CallEffect::Ignore,
    ),
    // ---- PtrDerive wrappers (pointer arithmetic + slice/vec pointer extraction) ----

    // Pointer arithmetic wrappers: constrain to `::ptr::` and method name.
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::byte_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::byte_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_byte_add",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::wrapping_byte_sub",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::cast_const",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ptr::",
        MatchKind::EndsWith,
        "::offset_from",
        CallEffect::Ignore,
    ),
    // Slice pointer extraction wrappers.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::as_mut_ptr",
        CallEffect::PtrDerive,
    ),
    // Raw pointer creation helpers.
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::from_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::from_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::UnsafeCell",
        MatchKind::EndsWith,
        "::get",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::SyncUnsafeCell",
        MatchKind::EndsWith,
        "::get",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::new",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::new_unchecked",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::NonNull",
        MatchKind::EndsWith,
        "::as_mut",
        CallEffect::PtrDerive,
    ),
    // Vec pointer extraction wrappers. Covers `alloc::vec::Vec` and `std::vec::Vec`, including monomorphized forms.
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::as_ptr",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::as_mut_ptr",
        CallEffect::PtrDerive,
    ),
    // Transmute-style helpers returning pointers should preserve lineage.
    EffectRule::one(
        MatchKind::Contains,
        "::mem::transmute",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::transmute",
        CallEffect::PtrDerive,
    ),
    // Volatile wrappers (free functions).
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::read_volatile",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::write_volatile",
        CallEffect::Store,
    ),
    // Volatile intrinsics.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::volatile_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::volatile_store",
        CallEffect::Store,
    ),
    // Core intrinsics used by mem::replace and similar wrappers.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::read_via_copy",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::write_via_move",
        CallEffect::Store,
    ),
    // Memset-like.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::write_bytes",
        CallEffect::MemSet,
    ),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::write_bytes)
    EffectRule::one(MatchKind::EndsWith, "::write_bytes", CallEffect::MemSet),
    // Plain wrappers.
    // Use suffix matching for `read`/`write` so we don't accidentally match `write_bytes`/`read_bytes`.
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::read_unaligned",
        CallEffect::LoadUnaligned,
    ),
    EffectRule::one(MatchKind::EndsWith, "::read", CallEffect::Load),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::write_unaligned",
        CallEffect::StoreUnaligned,
    ),
    EffectRule::one(MatchKind::EndsWith, "::drop_in_place", CallEffect::Store),
    EffectRule::two(
        MatchKind::Contains,
        "::cell::Cell",
        MatchKind::EndsWith,
        "::set",
        CallEffect::Store,
    ),
    EffectRule::one(MatchKind::EndsWith, "::write", CallEffect::Store),
    // Memcpy/memmove-like.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::copy",
        CallEffect::MemCopy,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    EffectRule::one(MatchKind::Contains, "::ptr::copy", CallEffect::MemCopy),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy_nonoverlapping)
    EffectRule::one(
        MatchKind::EndsWith,
        "::copy_nonoverlapping",
        CallEffect::MemCopy,
    ),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::copy)
    EffectRule::one(MatchKind::EndsWith, "::copy", CallEffect::MemCopy),
    // ---- Common pure helpers (Ignore) ----
    EffectRule::two(
        MatchKind::Contains,
        "::ops::RangeBounds",
        MatchKind::EndsWith,
        "::start_bound",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::RangeBounds",
        MatchKind::EndsWith,
        "::end_bound",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Range",
        MatchKind::EndsWith,
        "::contains",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::option::Option",
        MatchKind::EndsWith,
        "::expect",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::result::Result",
        MatchKind::EndsWith,
        "::is_ok",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::mem::size_of_val",
        CallEffect::Ignore,
    ),
    EffectRule::one(MatchKind::Contains, "::mem::take", CallEffect::Ignore),
    EffectRule::one(MatchKind::Contains, "::mem::replace", CallEffect::Ignore),
    EffectRule::one(
        MatchKind::Contains,
        "::panicking::assert_failed",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::PartialEq",
        MatchKind::EndsWith,
        "::eq",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::PartialOrd",
        MatchKind::EndsWith,
        "::partial_cmp",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::cmp::Ord",
        MatchKind::EndsWith,
        "::cmp",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::hash::Hash",
        MatchKind::EndsWith,
        "::hash",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::hash::impls",
        MatchKind::EndsWith,
        "::hash",
        CallEffect::Load,
    ),
    // AsRef/AsMut return borrowed pointers.
    EffectRule::two(
        MatchKind::Contains,
        "::convert::AsRef",
        MatchKind::EndsWith,
        "::as_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::convert::AsMut",
        MatchKind::EndsWith,
        "::as_mut",
        CallEffect::PtrDerive,
    ),
    // Index/IndexMut return references into the receiver.
    EffectRule::two(
        MatchKind::Contains,
        "::ops::IndexMut",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::ops::Index",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "SliceIndex",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "SliceIndex",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::index::<impl",
        MatchKind::EndsWith,
        "::index_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::index::<impl",
        MatchKind::EndsWith,
        "::index",
        CallEffect::PtrDerive,
    ),
    // Slice helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::get",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::get_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::last_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::split_at",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::split_at_mut",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::slice::<impl [",
        MatchKind::EndsWith,
        "::copy_from_slice",
        CallEffect::Ignore,
    ),
    // from_raw_parts{,_mut} return slice references derived from the base pointer.
    // This is heavily used by unsafe code; modeling it as PtrDerive avoids losing lineage.
    EffectRule::one(
        MatchKind::Contains,
        "::slice::from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::raw::from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::slice::raw::from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::slice_from_raw_parts",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::ptr::slice_from_raw_parts_mut",
        CallEffect::PtrDerive,
    ),
    // Vec helpers (metadata + length management).
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::capacity",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::set_len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::from_raw_parts",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::vec::Vec",
        MatchKind::EndsWith,
        "::from_raw_parts_in",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "alloc::slice::<impl [",
        MatchKind::EndsWith,
        "::to_vec",
        CallEffect::Ignore,
    ),
    // VecDeque helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::is_empty",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::collections::VecDeque",
        MatchKind::EndsWith,
        "::as_slices",
        CallEffect::CarrierCopyArg0,
    ),
    // String / str helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::str::<impl str>",
        MatchKind::EndsWith,
        "::len",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::str::<impl str>",
        MatchKind::EndsWith,
        "::as_bytes",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::string::String",
        MatchKind::EndsWith,
        "::as_bytes",
        CallEffect::Ignore,
    ),
    // IO helpers.
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::get_ref",
        CallEffect::PtrDerive,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::position",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::Cursor",
        MatchKind::EndsWith,
        "::set_position",
        CallEffect::Ignore,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::io::IoSlice",
        MatchKind::EndsWith,
        "::new",
        CallEffect::Ignore,
    ),
    // Pure state queries that only read scalar state from a receiver. These are common in parser
    // and codec hot paths and do not justify materializing a tracked temporary shared borrow.

    // Atomic ops (load/store vs RMW).
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::load",
        CallEffect::Load,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::store",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::swap",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::compare_exchange",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::compare_exchange_weak",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_add",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_sub",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_and",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_or",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_xor",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_nand",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_max",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::Atomic",
        MatchKind::EndsWith,
        "::fetch_min",
        CallEffect::Store,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::AtomicPtr",
        MatchKind::EndsWith,
        "::new",
        CallEffect::CarrierCopyArg0,
    ),
    EffectRule::two(
        MatchKind::Contains,
        "::sync::atomic::AtomicPtr",
        MatchKind::EndsWith,
        "::get_mut",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_store",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_compare_exchange",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_xadd",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::sync::atomic::atomic_xsub",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_load",
        CallEffect::Load,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_store",
        CallEffect::Store,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::atomic_",
        CallEffect::Store,
    ),
    // Intrinsics + pointer helpers seen in optimized builds.
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::arith_offset",
        CallEffect::PtrDerive,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::ptr_offset_from",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::ptr_offset_from_unsigned",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::compare_bytes",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::size_of_val",
        CallEffect::Ignore,
    ),
    EffectRule::one(
        MatchKind::Contains,
        "::intrinsics::align_of_val",
        CallEffect::Ignore,
    ),
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

mod logging;

#[derive(Clone, Debug)]
pub(in crate::instrumentation) enum SizeOperand<'tcx> {
    Const(Operand<'tcx>),
    SizeOf(Ty<'tcx>),
    AlignOf(Ty<'tcx>),
    ElemCount {
        elem_ty: Ty<'tcx>,
        count_op: Operand<'tcx>,
    },
    /// Size derived from wide-pointer metadata (slice length).
    PtrMetadataSlice {
        ptr_local: Local,
        elem_ty: Ty<'tcx>,
    },
    /// Size derived from wide-pointer metadata (str length).
    PtrMetadataStr {
        ptr_local: Local,
    },
    /// Size derived from wide-pointer metadata for a struct DST with trailing `[T]` field.
    /// Total bytes = offset_of(adt_ty, field_idx) + metadata * size_of::<elem_ty>().
    PtrMetadataAdtSlice {
        ptr_local: Local,
        adt_ty: Ty<'tcx>,
        field_idx: FieldIdx,
        elem_ty: Ty<'tcx>,
    },
}

#[derive(Clone, Debug)]
enum InstrKind<'tcx> {
    Ref {
        bk: BorrowKind,
        src: Place<'tcx>,
        projected_reborrow_anchor_key: Option<String>,
    },
    // Raw: created by MIR Rvalue::RawPtr; can propagate a parent tag from the source place.
    // Example MIR: `_p = &raw const (*_r);` where `_r: &u8`.
    Raw {
        is_mut: bool,
        src: Place<'tcx>,
    },
    // RawRoot: synthesized for pointer values without a thin-pointer source local
    // (e.g., transmute from NonNull/Unique, projected place, const/global pointer).
    // Example MIR: `_p = transmute::<NonNull<u8>, *const u8>(_nn);`.
    /// Root raw pointer creation for a pointer value already computed in a local.
    /// std/alloc often stores pointers inside ADTs like `NonNull<T>`/`Unique<T>` and then
    /// produces a thin pointer via `Transmute`. Our TagProp only propagates between thin pointer
    /// locals, so without this the destination pointer keeps tag=0 and triggers UNKNOWN_TAG.
    RawRoot {
        ptr_local: Local,
        is_mut: bool,
        exposed_provenance: bool,
    },
    /// Stack allocation lifetime event for a MIR local.
    StackAlloc {
        local: Local,
        live: bool,
        size_op: SizeOperand<'tcx>,
    },
    /// Heap allocation lifetime event for an allocator-returned pointer.
    /// `ptr_local` holds the pointer value; `size_op` is the allocation size operand (usize).
    HeapAlloc {
        ptr_local: Local,
        live: bool,
        size_op: SizeOperand<'tcx>,
    },
    /// Global/promoted const allocation materialized as a pointer.
    /// `ptr_local` holds the pointer value; `size` is the allocation size (0 = unknown).
    /// `base_offset` is the relative offset of the pointer within the global allocation.
    ConstAlloc {
        ptr_local: Local,
        size: usize,
        base_offset: usize,
    },
    /// Global/promoted const allocation from a constant pointer operand.
    /// Used when the pointer is not stored in a local (e.g., aggregate literals).
    ConstAllocConst {
        const_op: ConstOperand<'tcx>,
        size: usize,
        base_offset: usize,
    },
    /// A write through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrWrite {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A write through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrWriteAllowUntagged {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A write directly to a stack slot tracked via a reborrow anchor tag.
    /// Uses the allow-untagged runtime path so untouched locals do not report.
    StackSlotWriteAllowUntagged {
        local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A read through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrRead {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// A read through a pointer local, but skip if the tag is uninitialized (tag=0).
    PtrReadAllowUntagged {
        ptr_local: Local,
        size_op: SizeOperand<'tcx>,
        align_op: SizeOperand<'tcx>,
    },
    /// Coarse pointer-use tracking: a pointer-typed local appears in a call argument.
    /// This is treated as an escape event at call boundaries.
    PtrUse {
        ptr_local: Local,
    },
    /// Restore tag metadata for a pointer local loaded from a memory slot.
    ShadowLoad {
        dst_local: Local,
        require_tag: bool,
        validate_ref: bool,
    },
    /// Store tag metadata for a pointer local into a memory slot.
    ShadowStore {
        src_local: Local,
    },
    /// Store tag metadata for a pointer local into the heap pointee slot of a `Box<T>`.
    ///
    /// This is used for calls like `Box::new(p)` where `p` is itself pointer-typed. The pointer
    /// value is written into newly-allocated heap memory owned by the returned box, so we must
    /// also write the pointer's shadow metadata into that heap slot to preserve lineage for later
    /// loads such as `let q = *boxed_ptr`.
    ShadowStoreBoxPointee {
        box_local: Local,
        src_local: Local,
    },
    /// Copy tag metadata between memory slots.
    ShadowCopySlot {
        src_place: Place<'tcx>,
    },
    /// Copy tag metadata across a byte range between memory locations.
    ShadowCopyRange {
        src_place: Place<'tcx>,
        size_op: SizeOperand<'tcx>,
    },
    /// Clear pointer-shadow metadata for a written memory range.
    ShadowKill {
        size_op: SizeOperand<'tcx>,
    },
    /// Retire the current tag carried by a pointer/ref local whose MIR lifetime ended or which is
    /// being overwritten with a new pointer value.
    TagKill {
        ptr_local: Local,
    },
    /// Retire the current tag carried by a specific hidden tag local.
    TagLocalKill {
        tag_local: Local,
    },
    /// Retain the tag currently written into a hidden tag local so it stays live while any MIR
    /// local still carries that family.
    TagRetain {
        tag_local: Local,
    },
    /// Activate a source-level reference binding that rustc optimized onto a raw local.
    /// The resulting tag is synthetic: it is stored in `tag_local` and used for accesses
    /// through the raw local while the source scope is active.
    DebugRefActivate {
        raw_local: Local,
        tag_local: Local,
        is_mut: bool,
    },
    /// Propagate tags across pointer-to-pointer casts and plain copies/moves of pointer locals.
    /// This is a local tag assignment, not a runtime hook.
    TagProp {
        dst: Local,
        src: Local,
        copy_tag: bool,
        copy_ref_ancestor: bool,
    },
    /// Propagate both tag channels from the source ref-ancestor slot.
    /// Used when a stable SSA anchor is represented by a ref local whose
    /// semantic common parent is stored in `ref_ancestor`.
    TagPropFromRefAncestor {
        dst: Local,
        src: Local,
    },
    /// Reset a non-pointer local's exact-place reborrow anchor.
    ReborrowAnchorZero {
        anchor_local: Local,
        anchor_state_local: Option<Local>,
    },
    /// Initialize a non-pointer local's exact-place reborrow anchor from the first
    /// freshly-created ref tag for that place. Later reborrows must keep the original
    /// family anchor instead of overwriting it with newer child tags.
    ReborrowAnchorSet {
        dst_local: Local,
        anchor_local: Local,
        anchor_state_local: Option<Local>,
        src_ptr_local: Local,
    },
    /// Seed a non-pointer local's exact-place reborrow anchor from a recovered lineage source.
    ReborrowAnchorSeed {
        dst_local: Local,
        src_local: Local,
        mark_slot_family: bool,
    },
    /// Snapshot a parent lineage tag for a projected/non-local source place before a call.
    /// Used when the returned pointer value should derive from a call argument place, but the
    /// target block cannot reconstruct that parent from the call-site MIR anymore.
    ParentTagSnapshot {
        dst_local: Local,
        src: Place<'tcx>,
        is_raw_creation: bool,
    },
    /// Fresh tag for a derived pointer value (pointer arithmetic like add/sub/offset).
    /// Emits either ref/raw creation based on destination kind, with `parent=tag(src)`.
    PtrDerive {
        dst: Local,
        src: Local,
        is_mut: bool,
        is_ref: bool,
        strict_validity: bool,
    },
    /// Fresh tag for a derived pointer value using an explicit pre-call parent snapshot.
    PtrDeriveParent {
        dst: Local,
        is_mut: bool,
        is_ref: bool,
        strict_validity: bool,
    },
    /// Caller-side tag push for pointer arguments to a direct call.
    CallArgPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
        /// Bit 0 marks the custom-MIR exact in-place source shape `Move(*ptr)`.
        /// Bit 1 requests canonicalize-before-validate for recovered boundary families.
        flags: u8,
    },
    /// Caller-side validation for a by-value argument that is not itself pointer-typed,
    /// but carries a reference inside an aggregate/container.
    ///
    /// Examples:
    /// - `Option<&T>`
    /// - `(&T, bool)`
    /// - `struct Wrap<'a> { r: &'a T }`
    ///
    /// We do not push/take a call-boundary tag for these values because the ABI value is not a
    /// plain pointer local. Instead we recover the inner reference lineage from the carrier local
    /// and validate it immediately before the call.
    CallArgValidate {
        local: Local,
    },
    /// Callee-side retagging of pointer arguments from the runtime side-channel.
    ArgRetag {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Callee-side lineage anchor initialization for a non-pointer by-value argument.
    ///
    /// Used for argument carriers such as `Option<&T>`, tuples, or small wrapper structs when the
    /// callee local is not itself pointer-typed but still needs a stable reborrow-family anchor.
    /// The callee consumes the caller-pushed call-argument tag from the runtime side channel and
    /// stores it into the local anchor slot, so later inner-ref recovery does not fall back to
    /// `parent=0`.
    ArgAnchorTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
    }, // Callee-side validation for a return value that is not itself pointer-typed,
    /// but carries a reference inside an aggregate/container.
    ///
    /// Examples:
    /// - `Option<&T>`
    /// - `(&T, bool)`
    /// - `struct Wrap<'a> { r: &'a T }`
    ///
    /// Pointer returns use `RetPush`/`RetTake`. This hook exists for wrapper returns where the
    /// returned MIR local is not a plain pointer local, so we instead recover the inner reference
    /// lineage from `RETURN_PLACE` and validate it right before `Return`.
    RetValidate {
        callee_id: u64,
        local: Local,
    },
    /// Callee-side: export the exact shadow of one internal pointer leaf of a non-pointer return
    /// carrier right before `Return`.
    RetLeafPush {
        callee_id: u64,
        leaf_key: u64,
    },
    /// Callee-side: push the tag for a returned pointer right before `Return`.
    RetPush {
        callee_id: u64,
        ptr_local: Local,
    },
    /// Caller-side: take the pushed return tag after a call that returns a pointer.
    RetTake {
        callee_id: u64,
        dst_local: Local,
    },
    /// Caller-side: import the inner family exported by a non-pointer return carrier and seed the
    /// destination local's reborrow anchor.
    RetAnchorTake {
        callee_id: u64,
        local: Local,
    },
    /// Caller-side: restore the exact shadow for one internal pointer leaf of a non-pointer
    /// return carrier into the destination slot before control reaches the original call target.
    RetLeafTake {
        callee_id: u64,
        leaf_key: u64,
    },
    /// Caller-side: seed a fresh family for a returned owner/container carrier whose nested
    /// pointer fields are implementation detail rather than source-level borrow carriers.
    RetAnchorRoot {
        local: Local,
    },
    /// Callee-side: push the updated family for a `&mut T` carrier pointee slot on return.
    MutArgRetPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Callee-side: export the exact shadow of one internal pointer leaf of a `&mut T` carrier
    /// pointee so the caller can rebuild that slot shadow after the call returns.
    MutArgRetLeafPush {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
        leaf_key: u64,
    },
    /// Caller-side: take the callee-exported family for a `&mut T` carrier pointee slot.
    MutArgRetTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
        ptr_local: Local,
    },
    /// Caller-side: restore one internal pointer leaf shadow for a `&mut T` carrier pointee slot
    /// before control reaches the original call target.
    MutArgRetLeafTake {
        callee_id: u64,
        arg_index: u64,
        local: Local,
        leaf_key: u64,
    },
    /// Caller-side: take the callee-exported family for a live `&mut T` local when there is no
    /// separate carrier stack slot in this frame to anchor-refresh.
    ///
    /// This covers wrappers that forward their own `&mut T` argument into a nested call. The
    /// nested callee may retag the pointee-family, and the forwarding frame must refresh the
    /// still-live `&mut` local before using or re-exporting it again.
    MutArgRetTakePtrOnly {
        callee_id: u64,
        arg_index: u64,
        ptr_local: Local,
    },
    /// Caller-side: synthesize a fresh tag for an uninstrumented call return.
    RetRoot {
        dst_local: Local,
        is_mut: bool,
        is_ref: bool,
    },
    /// Callee-side: notify runtime alias models that this function is exiting.
    FnExit {
        callee_id: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentSelectionMode {
    ReceiverFamily,
    SlotFamily,
    PointeeFamily,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PtrStateLocals {
    tag_local: Local,
    ref_ancestor_local: Option<Local>,
    boundary_parent_local: Option<Local>,
    boundary_recovered_local: Option<Local>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CarrierSlotLocals {
    anchor_local: Local,
    slot_family_valid_local: Option<Local>,
}

const CALL_ARG_FLAG_INPLACE_EXACT_SOURCE: u8 = 1;
const CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE: u8 = 1 << 1;
const CALL_ARG_FLAG_USE_EXPORT_PARENT: u8 = 1 << 7;

#[derive(Clone, Debug)]
struct InsertPoint<'tcx> {
    bb: BasicBlock,
    stmt_idx: usize,
    insert_before: bool,
    source_info: SourceInfo,
    place: Place<'tcx>,
    kind: InstrKind<'tcx>,
}

#[derive(Copy, Clone, Debug)]
struct ShadowableLeafPtrSpec<'tcx> {
    place: Place<'tcx>,
    ty: Ty<'tcx>,
    byte_offset: Option<u64>,
    path_key: u64,
}

impl<'tcx> ShadowableLeafPtrSpec<'tcx> {
    const PATH_KEY_MARKER: u64 = 1 << 63;

    fn transport_key(self) -> u64 {
        self.byte_offset
            .unwrap_or(Self::PATH_KEY_MARKER | (self.path_key & !Self::PATH_KEY_MARKER))
    }
}

#[derive(Clone, Debug)]
struct ScanResult<'tcx> {
    insert_points: Vec<InsertPoint<'tcx>>,
    ptr_locals_needing_tag: HashSet<Local>,
    local_slot_shadow_store_locals: HashSet<Local>,
    projected_reborrow_anchor_specs: ReborrowAnchorSpecMap,
    projectionless_anchor_suppressed_locals: HashSet<Local>,
    interesting_stack_locals: HashSet<Local>,
    fallback_return_locals: Vec<(Local, SizeOperand<'tcx>)>,
    return_sites: Vec<(BasicBlock, SourceInfo, usize)>,
}

#[derive(Clone, Debug)]
struct HirRefBinding<'tcx> {
    name: rustc_span::Symbol,
    span: Span,
    ty: Ty<'tcx>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct DebugRefBindingKey {
    scope: SourceScope,
    raw_local: Local,
}

#[derive(Copy, Clone, Debug)]
struct DebugRefBinding {
    key: DebugRefBindingKey,
    tag_local: Local,
    is_mut: bool,
}

#[derive(Copy, Clone, Debug)]
struct ConstAllocInfo {
    size: usize,
    base_offset: usize,
}

#[derive(Copy, Clone, Debug)]
struct Hooks {
    def_id_ref: DefId,
    def_id_debug_ref: DefId,
    def_id_raw: DefId,
    def_id_alloc: DefId,
    def_id_write: DefId,
    def_id_write_allow_untagged: DefId,
    def_id_local_write_allow_untagged: DefId,
    def_id_read: DefId,
    def_id_read_allow_untagged: DefId,
    def_id_use: DefId,
    def_id_push_call_arg_tag: DefId,
    def_id_validate_call_arg_tag: DefId,
    def_id_take_call_arg_tag: DefId,
    def_id_take_call_arg_tag_anchor: DefId,
    def_id_push_ret_tag: DefId,
    def_id_validate_ret_tag: DefId,
    def_id_take_ret_tag: DefId,
    def_id_push_ret_leaf_shadow: DefId,
    def_id_take_ret_leaf_shadow: DefId,
    def_id_validate_loaded_ref_tag: DefId,
    def_id_require_loaded_ptr_tag: DefId,
    def_id_take_ret_tag_or_root: DefId,
    def_id_push_mut_arg_ret_tag: DefId,
    def_id_take_mut_arg_ret_tag: DefId,
    def_id_take_mut_arg_ret_tag_or_zero: DefId,
    def_id_push_mut_arg_ret_leaf_shadow: DefId,
    def_id_take_mut_arg_ret_leaf_shadow: DefId,
    def_id_exit_fn: DefId,
    def_id_shadow_store_ptr: DefId,
    def_id_shadow_store_ptr_local: DefId,
    def_id_shadow_load_tag: DefId,
    def_id_shadow_load_ref_ancestor: DefId,
    def_id_shadow_load_export_parent: DefId,
    def_id_shadow_load_export_parent_recovered: DefId,
    def_id_shadow_kill_range: DefId,
    def_id_tag_kill: DefId,
    def_id_tag_retain: DefId,
    def_id_shadow_copy_slot: DefId,
    def_id_shadow_copy_range: DefId,
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

    fn is_shadowable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        self.is_pointer_ty(ty)
            && (self.is_thin_ptr_ty(tcx, body, ty)
                || self.ptr_ty_has_precise_wide_bounds(tcx, body, ty))
    }

    /// Best-effort recursive check for whether `ty` contains any reference/raw-pointer field.
    ///
    /// This is used for non-pointer carrier values such as `Option<&T>`, tuples, or small wrapper
    /// structs so we can still add boundary validation/lineage handling even when the MIR local is
    /// not itself pointer-typed.
    fn ty_contains_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        if self.is_pointer_ty(ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    self.ty_contains_pointer_fields(tcx, body, field.ty(tcx, args), depth - 1)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.ty_contains_pointer_fields(tcx, body, *elem_ty, depth - 1)
            }
            _ => false,
        }
    }

    fn ty_is_direct_pointer_wrapper<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        let path = tcx.def_path_str(adt.did());
        let is_wrapper = path.contains("::sync::atomic::AtomicPtr")
            || path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::mem::MaybeUninit")
            || path.contains("::mem::ManuallyDrop")
            || path.contains("::cell::Cell");
        if !is_wrapper {
            return false;
        }
        adt.non_enum_variant().fields.iter().any(|field| {
            let field_ty = field.ty(tcx, args);
            self.is_pointer_ty(field_ty)
                || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, depth - 1)
        })
    }

    /// Shallow boundary-carrier check for source-level wrapper values that directly store a
    /// reference/raw pointer, such as `Option<&T>`, tuples of pointers, or small newtypes.
    ///
    /// This intentionally does *not* recurse through arbitrary nested ADTs like `Vec`, `RawVec`,
    /// `IntoIter`, or `NonNull`. Treating those owner/container internals as call-boundary
    /// borrow carriers produces false positives by transporting allocator/internal raw tags across
    /// moves and ABI copies where no source-level borrow is being passed.
    fn ty_contains_direct_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if self.is_pointer_ty(ty) {
            return true;
        }
        if self.ty_is_direct_pointer_wrapper(tcx, body, ty, 3) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                self.is_pointer_ty(field_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    let field_ty = field.ty(tcx, args);
                    self.is_pointer_ty(field_ty)
                        || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.is_pointer_ty(*elem_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, *elem_ty, 3)
            }
            _ => false,
        }
    }

    fn compile_alias_model_is_sb_like(&self) -> bool {
        std::env::var("RZ_ALIAS_MODEL")
            .ok()
            .map(|raw| raw.to_ascii_lowercase())
            .is_some_and(|model| matches!(model.as_str(), "sb" | "sb_lite" | "stacked_borrows"))
    }

    fn normalized_ptr_copy_anchor_key<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        rvalue: &Rvalue<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
    ) -> Option<(String, Vec<Local>)> {
        match rvalue {
            Rvalue::Use(Operand::Copy(place)) | Rvalue::Use(Operand::Move(place)) => {
                let place_ty = place.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(place_ty) {
                    return self.normalized_ptr_expr_key_for_ref_source_place(
                        body, *place, statements, upto,
                    );
                }
                None
            }
            Rvalue::CopyForDeref(place) => {
                self.normalized_ptr_expr_key_for_ref_source_place(body, *place, statements, upto)
            }
            _ => self.normalized_ptr_expr_key_for_rvalue(body, rvalue, statements, upto),
        }
    }

    /// Best-effort detection of "vtable-like" structs: all fields are function pointers.
    fn is_fn_table_adt_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
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
        if tagged_ptr_locals.contains(&ptr_local)
            || ptr_locals_with_tag_sources.contains(&ptr_local)
        {
            return;
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_pointer_ty(ptr_ty) {
            return;
        }

        let is_mut = self.ptr_is_mut(ptr_ty);
        let is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
        let projected_src =
            self.recover_projected_pointer_rhs_source(tcx, body, bb, stmt_idx, ptr_local);
        tagged_ptr_locals.insert(ptr_local);
        ptr_locals_needing_tag.insert(ptr_local);
        insert_points.push(InsertPoint {
            bb: projected_src.map_or(bb, |(def_bb, _, _)| def_bb),
            stmt_idx: projected_src.map_or(stmt_idx, |(_, def_stmt_idx, _)| def_stmt_idx),
            insert_before: projected_src.is_none(),
            source_info,
            place: Place::from(ptr_local),
            kind: if let Some((_def_bb, _def_stmt_idx, src_place)) = projected_src {
                if is_ref {
                    let bk = match ptr_ty.kind() {
                        TyKind::Ref(_, _, Mutability::Mut) => BorrowKind::Mut {
                            kind: MutBorrowKind::Default,
                        },
                        _ => BorrowKind::Shared,
                    };
                    InstrKind::Ref {
                        bk,
                        src: src_place,
                        projected_reborrow_anchor_key: None,
                    }
                } else {
                    InstrKind::Raw {
                        is_mut,
                        src: src_place,
                    }
                }
            } else if is_ref {
                InstrKind::RetRoot {
                    dst_local: ptr_local,
                    is_mut,
                    is_ref: true,
                }
            } else {
                InstrKind::RawRoot {
                    ptr_local,
                    is_mut,
                    exposed_provenance: false,
                }
            },
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
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(..) | Rvalue::RawPtr(..) => {
                        locals.insert(dst_local);
                    }
                    Rvalue::Use(op) => {
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
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
                        let mut has_tag_source = false;
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                has_tag_source = true;
                            }
                        }
                        if !has_tag_source
                            && matches!(rvalue, Rvalue::Cast(CastKind::Transmute, ..))
                        {
                            let src_ty = op.ty(body, tcx);
                            has_tag_source = match src_ty.kind() {
                                TyKind::Adt(adt, _) => {
                                    let name = tcx.def_path_str(adt.did());
                                    name.contains("::NonNull") || name.contains("::Unique")
                                }
                                _ => false,
                            };
                        }
                        if has_tag_source {
                            locals.insert(dst_local);
                        }
                    }
                    Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
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

    fn slot_addr_stmts_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
        is_mut: bool,
    ) -> Option<(Statement<'tcx>, Statement<'tcx>)> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if !place_ty.is_sized(tcx, body.typing_env(tcx)) {
            return None;
        }

        let raw_ptr_ty = if is_mut {
            Ty::new_mut_ptr(tcx, place_ty)
        } else {
            Ty::new_imm_ptr(tcx, place_ty)
        };
        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
            return None;
        }

        let slot_ptr_local = body
            .local_decls
            .push(LocalDecl::new(raw_ptr_ty, source_info.span));

        let slot_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(slot_ptr_local),
                Rvalue::RawPtr(
                    if is_mut {
                        RawPtrKind::Mut
                    } else {
                        RawPtrKind::Const
                    },
                    place,
                ),
            ))),
        );

        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(slot_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((slot_ptr_stmt, addr_stmt))
    }

    /// Return whether `local` has an explicit slot-family channel.
    ///
    /// Unlike `ty_contains_direct_pointer_fields`, this is allowed to recurse through owner or
    /// container internals. The slot-family models the borrow family of the outer slot `T`
    /// itself, not the ABI transport of any specific nested raw field. Types like `BytesMut`
    /// therefore need this path even though they do not store a source-level reference/raw
    /// directly.
    fn supports_slot_family_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        let local_ty = body.local_decls[local].ty;
        if !local_ty.is_sized(tcx, body.typing_env(tcx)) {
            return false;
        }
        if self.is_pointer_ty(local_ty) {
            return false;
        }
        if !self.ty_contains_pointer_fields(tcx, body, local_ty, 8) {
            return false;
        }

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
        self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty)
    }

    /// Return whether `local` should participate in by-value call/return carrier transport.
    ///
    /// This is intentionally narrower than `supports_arg_anchor_take_local`: only direct pointer
    /// carriers (for example `BytesMut`, `Option<&T>`, tuples/newtypes of pointers) qualify.
    /// Recursive owner/container internals like `Vec`, `Box`, `RawVec`, or `NonNull`-based
    /// wrappers are excluded here because transporting their allocator/raw internals across
    /// ordinary by-value calls can create false positives and, in optimized MIR, unstable
    /// call-boundary scaffolding.
    fn supports_call_boundary_anchor_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if !self.supports_slot_family_local(tcx, body, local) {
            return false;
        }
        let local_ty = body.local_decls[local].ty;
        if !self.ty_contains_direct_pointer_fields(tcx, body, local_ty) {
            return false;
        }

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
        self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty)
    }

    fn is_whole_place_slot_family_source<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> bool {
        src_place.projection.is_empty()
            && matches!(
                self.creation_parent_selection_mode_for_src_place(tcx, body, src_place, true),
                ParentSelectionMode::SlotFamily
            )
    }

    fn first_direct_ref_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> Option<(Place<'tcx>, Ty<'tcx>, bool)> {
        let local_ty = body.local_decls[local].ty;
        let field_tys = self.aggregate_field_tys(tcx, local_ty)?;
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            let TyKind::Ref(_, pointee_ty, mutbl) = field_ty.kind() else {
                continue;
            };
            let field_place = self.pointer_field_place(tcx, local, field_idx, field_ty);
            return Some((field_place, *pointee_ty, matches!(mutbl, Mutability::Mut)));
        }
        None
    }

    fn first_direct_pointer_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> Option<Place<'tcx>> {
        let local_ty = body.local_decls[local].ty;
        let field_tys = self.aggregate_field_tys(tcx, local_ty)?;
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            if self.is_shadowable_ptr_ty(tcx, body, field_ty) {
                return Some(self.pointer_field_place(tcx, local, field_idx, field_ty));
            }
        }
        None
    }

    fn leaf_path_key_child(parent_key: u64, field_idx: usize) -> u64 {
        parent_key
            .wrapping_mul(131)
            .wrapping_add(field_idx as u64 + 1)
    }

    fn collect_shadowable_leaf_ptr_specs_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        depth: usize,
        byte_offset: Option<u64>,
        path_key: u64,
        out: &mut Vec<ShadowableLeafPtrSpec<'tcx>>,
    ) {
        let place_ty = base_place.ty(&body.local_decls, tcx);
        let ty = place_ty.ty;
        if self.is_shadowable_ptr_ty(tcx, body, ty) {
            out.push(ShadowableLeafPtrSpec {
                place: base_place,
                ty,
                byte_offset,
                path_key,
            });
            return;
        }
        if depth == 0 {
            return;
        }
        let Some(field_tys) = self.aggregate_field_tys(tcx, ty) else {
            return;
        };
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            if !self.is_shadowable_ptr_ty(tcx, body, field_ty)
                && !self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)
            {
                continue;
            }
            let field_place =
                self.pointer_field_place_from_place(tcx, base_place, field_idx, field_ty);
            let field_offset = self.field_offset_bytes(
                tcx,
                body,
                place_ty.ty,
                place_ty.variant_index,
                FieldIdx::from_usize(field_idx),
            );
            let child_offset = match (byte_offset, field_offset) {
                (Some(base), Some(field)) => Some(base.wrapping_add(field)),
                _ => None,
            };
            self.collect_shadowable_leaf_ptr_specs_from_place(
                tcx,
                body,
                field_place,
                depth - 1,
                child_offset,
                Self::leaf_path_key_child(path_key, field_idx),
                out,
            );
        }
    }

    fn shadowable_leaf_ptr_specs_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        _ty: Ty<'tcx>,
    ) -> Vec<ShadowableLeafPtrSpec<'tcx>> {
        let mut out = Vec::new();
        self.collect_shadowable_leaf_ptr_specs_from_place(
            tcx,
            body,
            base_place,
            3,
            Some(0),
            0,
            &mut out,
        );
        out
    }

    fn shadowable_leaf_ptr_places_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        ty: Ty<'tcx>,
    ) -> Vec<(Place<'tcx>, Ty<'tcx>)> {
        self.shadowable_leaf_ptr_specs_from_place(tcx, body, base_place, ty)
            .into_iter()
            .map(|spec| (spec.place, spec.ty))
            .collect()
    }

    fn raw_creation_allows_no_provenance_transport<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
    ) -> bool {
        if !matches!(src.projection.first(), Some(ProjectionElem::Deref)) {
            return false;
        }

        let mut cur_place = Place::from(src.local);
        for proj in src.projection.iter() {
            let cur_ty = cur_place.ty(&body.local_decls, tcx).ty;
            if let ProjectionElem::Field(_, field_ty) = proj {
                if !self.is_pointer_ty(cur_ty)
                    && (self.is_pointer_ty(field_ty)
                        || self.ty_contains_pointer_fields(tcx, body, field_ty, 2))
                {
                    return true;
                }
            }
            cur_place = cur_place.project_deeper(&[proj], tcx);
        }

        false
    }

    fn pair_shadowable_leaf_ptr_specs<'tcx>(
        &self,
        dst_specs: &[ShadowableLeafPtrSpec<'tcx>],
        src_specs: &[ShadowableLeafPtrSpec<'tcx>],
    ) -> Option<Vec<(ShadowableLeafPtrSpec<'tcx>, ShadowableLeafPtrSpec<'tcx>)>> {
        if dst_specs.is_empty() || dst_specs.len() != src_specs.len() {
            return None;
        }

        if dst_specs.iter().all(|spec| spec.byte_offset.is_some())
            && src_specs.iter().all(|spec| spec.byte_offset.is_some())
        {
            let mut src_by_offset: HashMap<u64, ShadowableLeafPtrSpec<'tcx>> = HashMap::new();
            for src_spec in src_specs.iter().copied() {
                let offset = src_spec.byte_offset.expect("checked above");
                if src_by_offset.insert(offset, src_spec).is_some() {
                    src_by_offset.clear();
                    break;
                }
            }
            if !src_by_offset.is_empty() {
                let mut pairs = Vec::with_capacity(dst_specs.len());
                let mut matched_all = true;
                for dst_spec in dst_specs.iter().copied() {
                    let offset = dst_spec.byte_offset.expect("checked above");
                    let Some(src_spec) = src_by_offset.remove(&offset) else {
                        matched_all = false;
                        break;
                    };
                    pairs.push((dst_spec, src_spec));
                }
                if matched_all && src_by_offset.is_empty() {
                    return Some(pairs);
                }
            }
        }

        let mut src_by_key: HashMap<u64, ShadowableLeafPtrSpec<'tcx>> = HashMap::new();
        let mut duplicate_key = false;
        for src_spec in src_specs.iter().copied() {
            if src_by_key
                .insert(src_spec.transport_key(), src_spec)
                .is_some()
            {
                duplicate_key = true;
                break;
            }
        }
        if !duplicate_key {
            let mut pairs = Vec::with_capacity(dst_specs.len());
            let mut matched_all = true;
            for dst_spec in dst_specs.iter().copied() {
                let Some(src_spec) = src_by_key.remove(&dst_spec.transport_key()) else {
                    matched_all = false;
                    break;
                };
                pairs.push((dst_spec, src_spec));
            }
            if matched_all && src_by_key.is_empty() {
                return Some(pairs);
            }
        }

        None
    }

    /// Pair return-value shadow leaves with the pointer/view provenance of `arg0`.
    ///
    /// This handles view constructors like `split_at{,_mut}` and `VecDeque::as_slices`, where
    /// one pointer-bearing input produces an aggregate return with multiple pointer/view leaves.
    /// We first try the normal 1:1 structural pairing. If that fails and `arg0` contributes a
    /// single shadowable leaf, we fan that one source leaf out to every returned leaf.
    fn pair_shadowable_leaf_ptr_specs_from_arg0<'tcx>(
        &self,
        dst_specs: &[ShadowableLeafPtrSpec<'tcx>],
        src_specs: &[ShadowableLeafPtrSpec<'tcx>],
    ) -> Option<Vec<(ShadowableLeafPtrSpec<'tcx>, ShadowableLeafPtrSpec<'tcx>)>> {
        if let Some(pairs) = self.pair_shadowable_leaf_ptr_specs(dst_specs, src_specs) {
            return Some(pairs);
        }

        if dst_specs.is_empty() || src_specs.len() != 1 {
            return None;
        }

        let src_spec = src_specs[0];
        Some(
            dst_specs
                .iter()
                .copied()
                .map(|dst_spec| (dst_spec, src_spec))
                .collect(),
        )
    }

    /// Compute MIR statements that recover the heap payload address stored inside a `Box<T>` local.
    ///
    /// This is specifically for `ShadowStoreBoxPointee`: after a call such as `Box::new(p)`, we
    /// need the address of the box pointee storage so we can write `p`'s shadow tag metadata into
    /// that heap slot. The helper walks the `Box<T>` representation down to its internal
    /// `NonNull<T>`, converts it to a raw byte pointer, and then exposes provenance to obtain the
    /// slot address as `usize`.
    fn box_pointee_slot_addr_stmts_for_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        box_local: Local,
        addr_local: Local,
    ) -> Option<(Statement<'tcx>, Statement<'tcx>)> {
        let box_ty = body.local_decls[box_local].ty;
        let TyKind::Adt(box_adt, box_args) = box_ty.kind() else {
            return None;
        };
        if !self.is_box_ty(tcx, box_ty) {
            return None;
        }

        let unique_ty =
            box_adt.non_enum_variant().fields[FieldIdx::from_usize(0)].ty(tcx, box_args);
        let TyKind::Adt(unique_adt, unique_args) = unique_ty.kind() else {
            return None;
        };
        let nonnull_ty =
            unique_adt.non_enum_variant().fields[FieldIdx::from_usize(0)].ty(tcx, unique_args);

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, tcx.types.u8);
        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
            return None;
        }

        let nonnull_place = Place::from(box_local).project_deeper(
            &[
                PlaceElem::Field(FieldIdx::from_usize(0), unique_ty),
                PlaceElem::Field(FieldIdx::from_usize(0), nonnull_ty),
            ],
            tcx,
        );
        let slot_ptr_local = body
            .local_decls
            .push(LocalDecl::new(raw_ptr_ty, source_info.span));

        let slot_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(slot_ptr_local),
                Rvalue::Cast(
                    CastKind::Transmute,
                    Operand::Copy(nonnull_place),
                    raw_ptr_ty,
                ),
            ))),
        );
        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(slot_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((slot_ptr_stmt, addr_stmt))
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
            | InstrKind::RetRoot {
                dst_local: ptr_local,
                ..
            }
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
            stats
                .access_hooks_before
                .saturating_sub(stats.access_hooks_after),
            stats.hooks_total_before,
            stats.hooks_total_after,
            stats
                .hooks_total_before
                .saturating_sub(stats.hooks_total_after)
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
                    crate_name, callee, seed_unknown, backward_unknown,
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
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-summary] failed to serialize summary"
            );
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

    fn callee_id_u64<'tcx>(&self, tcx: TyCtxt<'tcx>, def_id: DefId) -> u64 {
        tcx.def_path_hash(def_id).0.to_smaller_hash().as_u64()
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
            let flag = if allow.contains(&name) {
                "instrumented"
            } else {
                "dep"
            };
            eprintln!("  - {} ({})", name, flag);
        }
        eprintln!(
            "[rusteze] note: current crate is always treated as instrumented; set RZ_INSTRUMENTED_CRATES or RZ_INSTRUMENT_ALL_DEPS=1 to include dependencies."
        );
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

        for (bb, block_data) in traversal::preorder(body) {
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

    fn local_has_observable_use_excluding_call_dest<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
        call_dest_loc: Location,
    ) -> bool {
        let mut finder = LocalUseFinder {
            local,
            skip_call_dest: Some(call_dest_loc),
            found: false,
        };

        for (bb, block_data) in traversal::preorder(body) {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                finder.visit_statement(
                    stmt,
                    Location {
                        block: bb,
                        statement_index: stmt_idx,
                    },
                );
                if finder.found {
                    return true;
                }
            }
            finder.visit_terminator(
                block_data.terminator(),
                Location {
                    block: bb,
                    statement_index: block_data.statements.len(),
                },
            );
            if finder.found {
                return true;
            }
        }
        false
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
            } = &term.kind
            else {
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

    fn noescape_shared_reborrow_call_temp_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        func: &Operand<'tcx>,
        local_ref_use_stats: &HashMap<Local, LocalRefUseStats>,
        arg_index: usize,
        arg: &Spanned<Operand<'tcx>>,
    ) -> Option<Local> {
        let Some(place) = self.place_from_operand(&arg.node) else {
            return None;
        };
        if !place.projection.is_empty() {
            return None;
        }
        let local = place.local;
        let local_ty = body.local_decls[local].ty;
        if !matches!(local_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
            return None;
        }
        let stat = local_ref_use_stats.get(&local)?;
        if stat.defs != 1 || stat.uses != 1 {
            return None;
        }

        let mut def_src_place: Option<Place<'tcx>> = None;
        for bbd in body.basic_blocks.iter() {
            for stmt in &bbd.statements {
                if let StatementKind::Assign(box (
                    lhs,
                    Rvalue::Ref(_, BorrowKind::Shared, src_place),
                )) = &stmt.kind
                {
                    if lhs.as_local() == Some(local) {
                        def_src_place = Some(*src_place);
                    }
                }
            }
        }
        let src_place = def_src_place?;
        let whole_place_slot_family_src =
            self.is_whole_place_slot_family_source(tcx, body, src_place);
        if !matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && !whole_place_slot_family_src
        {
            return None;
        }

        let Some((callee_did, _)) = self.direct_callee(tcx, body, block_data, func) else {
            return None;
        };
        let summary_allows = unsafe_dataflow::summary_for_def_id(tcx, callee_did).and_then(
            |summary| {
                summary
                    .ptr_args()
                    .iter()
                    .find(|entry| entry.arg_index() == arg_index)
                    .map(|arg_summary| {
                        !arg_summary.reaches_direct_sink()
                            && !arg_summary.escapes_to_unknown_boundary()
                            && !arg_summary.forwarded_to_return()
                    })
            },
        );
        if !matches!(summary_allows, Some(true)) {
            return None;
        }

        Some(local)
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
                if after.contains("::") { s } else { &s[..pos] }
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
                    MatchKind::Contains => def_path.contains(n2) || def_path_norm.contains(n2),
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

    fn collect_hir_ref_bindings<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> Vec<HirRefBinding<'tcx>> {
        let Some(local_def_id) = body.source.def_id().as_local() else {
            return Vec::new();
        };
        if !tcx.hir_body_owner_kind(local_def_id).is_fn_or_closure() {
            return Vec::new();
        }
        let Some(hir_body) = tcx.hir_maybe_body_owned_by(local_def_id) else {
            return Vec::new();
        };
        let typeck = tcx.typeck(local_def_id);
        let mut collector = HirRefBindingCollector {
            typeck,
            bindings: Vec::new(),
        };
        collector.visit_body(hir_body);
        collector.bindings
    }

    fn collect_debug_ref_bindings<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> Vec<(DebugRefBindingKey, bool)> {
        let hir_bindings = self.collect_hir_ref_bindings(tcx, body);
        if hir_bindings.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<(DebugRefBindingKey, bool)> = Vec::new();
        for info in &body.var_debug_info {
            let VarDebugInfoContents::Place(place) = info.value else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let raw_local = place.local;
            if !matches!(body.local_decls[raw_local].ty.kind(), TyKind::RawPtr(..)) {
                continue;
            }

            let scope = info.source_info.scope;
            let scope_span = body.source_scopes[scope].span;
            let Some(binding) = hir_bindings
                .iter()
                .filter(|binding| binding.name == info.name)
                .filter(|binding| {
                    span_contains(scope_span, binding.span)
                        || span_contains(binding.span, scope_span)
                        || span_contains(info.source_info.span, binding.span)
                        || span_contains(binding.span, info.source_info.span)
                })
                .min_by_key(|binding| binding.span.hi().0 - binding.span.lo().0)
            else {
                continue;
            };

            let TyKind::Ref(_, _, mutbl) = binding.ty.kind() else {
                continue;
            };

            let key = DebugRefBindingKey { scope, raw_local };
            let is_mut = matches!(mutbl, Mutability::Mut);
            if !out.iter().any(|(existing, _)| *existing == key) {
                out.push((key, is_mut));
            }
        }

        out
    }

    fn scope_is_within<'tcx>(
        &self,
        body: &Body<'tcx>,
        mut current: SourceScope,
        target: SourceScope,
    ) -> bool {
        loop {
            if current == target {
                return true;
            }
            let Some(parent) = body.source_scopes[current].parent_scope else {
                return false;
            };
            current = parent;
        }
    }

    fn scope_entry_locations<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
    ) -> Vec<(BasicBlock, usize, SourceInfo)> {
        let predecessors = body.basic_blocks.predecessors();
        let mut out = Vec::new();
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            let mut first_loc: Option<(usize, SourceInfo)> = None;
            for (stmt_idx, stmt) in data.statements.iter().enumerate() {
                if !self.scope_is_within(body, stmt.source_info.scope, scope) {
                    continue;
                }
                first_loc = Some((stmt_idx, stmt.source_info));
                break;
            }

            if first_loc.is_none() {
                if let Some(term) = data.terminator.as_ref() {
                    if self.scope_is_within(body, term.source_info.scope, scope) {
                        first_loc = Some((data.statements.len(), term.source_info));
                    }
                }
            }

            let Some((stmt_idx, source_info)) = first_loc else {
                continue;
            };

            let enters_scope = predecessors[bb].iter().all(|pred| {
                body.basic_blocks[*pred]
                    .terminator
                    .as_ref()
                    .map(|term| !self.scope_is_within(body, term.source_info.scope, scope))
                    .unwrap_or(true)
            });
            if enters_scope {
                out.push((bb, stmt_idx, source_info));
            }
        }
        out
    }

    fn debug_ref_activation_locations<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
        raw_local: Local,
    ) -> Vec<(BasicBlock, usize, SourceInfo)> {
        let mut out = Vec::new();
        for (bb, entry_stmt_idx, entry_source_info) in self.scope_entry_locations(body, scope) {
            let block = &body.basic_blocks[bb];
            let mut activation = None;
            for (stmt_idx, stmt) in block.statements[..entry_stmt_idx].iter().enumerate().rev() {
                let StatementKind::Assign(box (dst_place, _)) = &stmt.kind else {
                    continue;
                };
                if dst_place.as_local() != Some(raw_local) {
                    continue;
                }
                activation = Some((bb, stmt_idx, stmt.source_info));
                break;
            }
            out.push(activation.unwrap_or((bb, entry_stmt_idx, entry_source_info)));
        }
        out.sort_unstable_by_key(|(bb, stmt_idx, _)| (bb.index(), *stmt_idx));
        out.dedup_by_key(|(bb, stmt_idx, _)| (bb.index(), *stmt_idx));
        out
    }

    fn active_debug_ref_binding_tag_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        scope: SourceScope,
        access_span: Span,
        raw_local: Local,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
    ) -> Option<Local> {
        let mut cur = Some(scope);
        while let Some(scope) = cur {
            let key = DebugRefBindingKey { scope, raw_local };
            if let Some(binding) = debug_ref_bindings.get(&key) {
                return Some(binding.tag_local);
            }
            cur = body.source_scopes[scope].parent_scope;
        }
        let mut best: Option<(u32, Local)> = None;
        for (key, binding) in debug_ref_bindings.iter() {
            if key.raw_local != raw_local {
                continue;
            }
            let binding_span = body.source_scopes[key.scope].span;
            if !span_contains(binding_span, access_span) {
                continue;
            }
            let len = binding_span.hi().0 - binding_span.lo().0;
            match best {
                Some((best_len, _)) if best_len <= len => {}
                _ => best = Some((len, binding.tag_local)),
            }
        }
        if let Some((_, tag_local)) = best {
            return Some(tag_local);
        }
        None
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

    fn align_operand_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let typing_env = body.typing_env(tcx);
        if ty.is_sized(tcx, typing_env) {
            return SizeOperand::AlignOf(ty);
        }
        match ty.kind() {
            TyKind::Slice(elem_ty) => self.align_operand_for_ty(tcx, body, *elem_ty, span),
            TyKind::Str => SizeOperand::Const(self.const_usize(tcx, span, 1)),
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
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

    /// Stack-slot tracking should cover ordinary fixed-size locals as well as dynamic ones.
    /// The only stack sizes we intentionally suppress are the synthetic `size=0` operands used
    /// by the pass as "unknown/unsupported stack extent" sentinels.
    fn should_emit_stack_alloc_for_size_op<'tcx>(&self, size_op: &SizeOperand<'tcx>) -> bool {
        match size_op {
            SizeOperand::Const(Operand::Constant(c)) => !matches!(
                c.const_,
                Const::Val(ConstValue::Scalar(Scalar::Int(int)), _)
                    if int.to_bits(int.size()) == 0
            ),
            SizeOperand::Const(_) => true,
            _ => true,
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
            TyKind::Adt(..) => match self.adt_slice_tail(tcx, body, pointee) {
                Some((field_idx, elem_ty)) => SizeOperand::PtrMetadataAdtSlice {
                    ptr_local,
                    adt_ty: pointee,
                    field_idx,
                    elem_ty,
                },
                None => SizeOperand::Const(self.const_usize(tcx, span, 0)),
            },
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    fn align_operand_for_deref<'tcx>(
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
        self.align_operand_for_ty(tcx, body, pointee, span)
    }

    fn align_operand_for_ptr_local<'tcx>(
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
        self.align_operand_for_ty(tcx, body, pointee, span)
    }

    fn align_operand_for_ptr_derive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Local,
        dst: Local,
        is_ref: bool,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if is_ref {
            let dst_ty = body.local_decls[dst].ty;
            if matches!(
                dst_ty.kind(),
                TyKind::Ref(_, pointee, _) if matches!(pointee.kind(), TyKind::Dynamic(..))
            ) {
                return self.align_operand_for_ptr_local(tcx, body, src, span);
            }
        }
        self.align_operand_for_ptr_local(tcx, body, dst, span)
    }

    fn align_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let mut place_ty = PlaceTy::from_ty(body.local_decls[src.local].ty);
        for proj in src.projection.iter() {
            if let ProjectionElem::Field(..) = proj {
                if let TyKind::Adt(adt_def, _) = place_ty.ty.kind() {
                    if adt_def.repr().packed() {
                        return SizeOperand::Const(self.const_usize(tcx, span, 1));
                    }
                }
            }
            if matches!(proj, ProjectionElem::Deref) {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
            place_ty = place_ty.projection_ty(tcx, proj.clone());
        }
        self.align_operand_for_ty(tcx, body, place_ty.ty, span)
    }

    fn align_operand_for_ref_creation_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        dst_ty: Ty<'tcx>,
        src: Place<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let src_ty = src.ty(&body.local_decls, tcx).ty;
        match src_ty.kind() {
            // When we copy/load an existing pointer value into a new `&T`, the creation hook
            // must validate the loaded reference against the pointee alignment, not the source
            // slot alignment of the reference object itself.
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if self.place_may_cross_packed_field(tcx, body, src) {
                    SizeOperand::Const(self.const_usize(tcx, span, 1))
                } else if matches!(
                    dst_ty.kind(),
                    TyKind::Ref(_, dst_pointee, _) if *dst_pointee == src_ty
                ) {
                    self.align_operand_for_src_place(tcx, body, src, span)
                } else {
                    self.align_operand_for_ty(tcx, body, *pointee, span)
                }
            }
            _ => self.align_operand_for_src_place(tcx, body, src, span),
        }
    }

    fn place_may_cross_packed_field<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
    ) -> bool {
        let mut place_ty = PlaceTy::from_ty(body.local_decls[src.local].ty);
        for proj in src.projection.iter() {
            if let ProjectionElem::Field(..) = proj {
                if let TyKind::Adt(adt_def, _) = place_ty.ty.kind() {
                    if adt_def.repr().packed() {
                        return true;
                    }
                }
            }
            place_ty = place_ty.projection_ty(tcx, proj.clone());
        }
        false
    }

    /// For a struct DST with a trailing `[T]` field, return the field index and element type.
    /// Other DST forms (`dyn Trait`, nested DST) return None.
    fn adt_slice_tail<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        adt_ty: Ty<'tcx>,
    ) -> Option<(FieldIdx, Ty<'tcx>)> {
        let (adt_def, substs) = match adt_ty.kind() {
            TyKind::Adt(adt_def, substs) if adt_def.is_struct() => (adt_def, substs),
            _ => return None,
        };
        let variant = adt_def.non_enum_variant();
        let last_idx = variant.fields.len().checked_sub(1)?;
        let field = &variant.fields[FieldIdx::from_usize(last_idx)];
        let field_ty = field.ty(tcx, substs);
        let typing_env = body.typing_env(tcx);
        if field_ty.is_sized(tcx, typing_env) {
            return None;
        }
        match field_ty.kind() {
            TyKind::Slice(elem_ty) => Some((FieldIdx::from_usize(last_idx), *elem_ty)),
            _ => None,
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
            TyKind::Adt(..) => match self.adt_slice_tail(tcx, body, pointee) {
                Some((field_idx, elem_ty)) => SizeOperand::PtrMetadataAdtSlice {
                    ptr_local,
                    adt_ty: pointee,
                    field_idx,
                    elem_ty,
                },
                None => SizeOperand::Const(self.const_usize(tcx, span, 0)),
            },
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
            TyKind::Adt(..) if !pointee.is_sized(tcx, body.typing_env(tcx)) => {
                match self.adt_slice_tail(tcx, body, pointee) {
                    Some((field_idx, elem_ty)) => SizeOperand::PtrMetadataAdtSlice {
                        ptr_local,
                        adt_ty: pointee,
                        field_idx,
                        elem_ty,
                    },
                    None => SizeOperand::Const(self.const_usize(tcx, span, 0)),
                }
            }
            _ if pointee.is_sized(tcx, body.typing_env(tcx)) => {
                self.size_operand_for_ty(tcx, body, pointee, span)
            }
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    fn ptr_ty_has_precise_wide_bounds<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str) {
                    return true;
                }
                if matches!(pointee.kind(), TyKind::Adt(..)) {
                    return self.adt_slice_tail(tcx, body, *pointee).is_some();
                }
                false
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
        self.ptr_ty_has_precise_wide_bounds(tcx, body, src_ty)
            && self.is_thin_ptr_ty(tcx, body, dst_ty)
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
        let input = PseudoCanonicalInput {
            typing_env,
            value: base_ty,
        };
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

        let mut ensure_offset_local =
            |body: &mut Body<'tcx>, stmts: &mut Vec<Statement<'tcx>>| -> Local {
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
                    let offset_bytes = self.field_offset_bytes(
                        tcx,
                        body,
                        place_ty.ty,
                        place_ty.variant_index,
                        *field,
                    )?;
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
                                        self.const_usize(
                                            tcx,
                                            source_info.span,
                                            offset_bytes as usize,
                                        ),
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
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
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
                ProjectionElem::ConstantIndex {
                    offset, from_end, ..
                } => {
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
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
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
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
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
    ) -> Option<(
        Option<Statement<'tcx>>,
        Statement<'tcx>,
        Vec<Statement<'tcx>>,
    )> {
        let has_deref = place
            .projection
            .iter()
            .next()
            .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
        if has_deref {
            let base_place = Place::from(place.local);
            let (opt, stmt) =
                self.addr_stmts_for_place(tcx, body, source_info, base_place, addr_local)?;
            let offset_stmts = self.offset_stmts_for_projection(
                tcx,
                body,
                source_info,
                place.local,
                &place.projection[1..],
                addr_local,
            )?;
            return Some((opt, stmt, offset_stmts));
        }

        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_pointer_ty(place_ty) {
            let (opt, stmt) =
                self.addr_stmts_for_place(tcx, body, source_info, place, addr_local)?;
            return Some((opt, stmt, Vec::new()));
        }

        None
    }

    /// Returns true when alias checks should be skipped for an accessed memory type.
    /// This is intentionally broad: accesses that touch an aggregate containing
    /// interior-mutability must stay exempt to avoid attributing writes to the
    /// wrong subfield.
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

    /// Returns true when a pointee type is itself an interior-mutability root that should
    /// propagate alias exemption to a directly-derived child pointer.
    ///
    /// This is narrower than `alias_exempt_for_ty`: aggregates that merely *contain*
    /// an `UnsafeCell` (for example a struct with one `Cell` field) are not treated as
    /// alias-exempt roots for derived tags, otherwise a raw/shared tag to the whole
    /// aggregate would suppress alias checks for unrelated non-interior-mutable fields.
    fn alias_exempt_root_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
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
        if ty.is_freeze(tcx, typing_env) {
            return false;
        }

        match ty.kind() {
            TyKind::Adt(adt, _) => {
                let path = tcx.def_path_str(adt.did());
                path.contains("::cell::UnsafeCell")
                    || path.contains("::cell::SyncUnsafeCell")
                    || path.contains("::cell::Cell")
                    || path.contains("::cell::RefCell")
                    || path.contains("::pin::UnsafePinned")
            }
            TyKind::Tuple(_) | TyKind::Array(..) | TyKind::Slice(_) => false,
            _ => true,
        }
    }

    fn alias_exempt_for_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, mutbl) => {
                if matches!(mutbl, Mutability::Mut) {
                    false
                } else {
                    self.alias_exempt_root_for_ty(tcx, body, *pointee)
                }
            }
            TyKind::RawPtr(pointee, _) => self.alias_exempt_root_for_ty(tcx, body, *pointee),
            _ => false,
        }
    }

    fn tb_call_arg_protector_supported_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, Mutability::Mut) => pointee.is_unpin(tcx, body.typing_env(tcx)),
            _ => true,
        }
    }

    /// Preserve alias exemption for direct derivations out of an interior-mutability root
    /// (`UnsafeCell<T>`, `Cell<T>`, etc.) when the child pointee still fits entirely within
    /// the source storage.
    ///
    /// This keeps wrappers like `UnsafeCell::get()` exempt, while avoiding false negatives
    /// when a non-root aggregate or a zero-sized interior-mutable wrapper is cast to some
    /// unrelated byte/field pointer.
    fn alias_exempt_child_from_source_ptr<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_ty: Ty<'tcx>,
        dst_ptr_ty: Ty<'tcx>,
    ) -> bool {
        let src_pointee = match src_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return false,
        };
        let dst_pointee = match dst_ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return false,
        };

        if !self.alias_exempt_root_for_ty(tcx, body, src_pointee) {
            return false;
        }

        let src_size = self.layout_size_bytes(tcx, src_pointee);
        let dst_size = self.layout_size_bytes(tcx, dst_pointee);
        src_size != 0 && dst_size != 0 && dst_size <= src_size
    }

    /// Return the alias-exempt classification for the memory location accessed by `ty`.
    ///
    /// For pointer-typed access places (`&T`, `*mut T`) the access is to the pointee, not to the
    /// pointer value itself. For projected writes like `(*p).field = ...`, `ty` is already the
    /// field type, which lets us keep interior-mutable fields alias-exempt without exempting the
    /// entire aggregate.
    fn alias_exempt_for_access_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                self.alias_exempt_for_ty(tcx, body, *pointee)
            }
            _ => self.alias_exempt_for_ty(tcx, body, ty),
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

    fn is_one_byte_sized_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        self.layout_size_bytes(tcx, ty) == 1
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

    fn mir_const_needs_normalization<'tcx>(&self, c: rustc_middle::mir::Const<'tcx>) -> bool {
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

        if matches!(c, rustc_middle::mir::Const::Unevaluated(..)) {
            return true;
        }

        let mut v = NeedsNormalizationVisitor;
        c.visit_with(&mut v).is_break()
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
            || self.mir_const_needs_normalization(c.const_)
        {
            return None;
        }

        // Some dependency graphs (for example `mail-internals` via `object`) still trigger
        // rustc normalization ICEs while evaluating pointer-valued MIR constants with
        // projection-heavy associated types. Unknown const allocation info is acceptable for our
        // instrumentation, so treat those consts conservatively instead of crashing the compiler.
        let scalar = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.const_
                .try_eval_scalar(tcx, TypingEnv::fully_monomorphized())
        }))
        .ok()
        .flatten()?;
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
            || self.mir_const_needs_normalization(c.const_)
        {
            return None;
        }

        let scalar = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.const_
                .try_eval_scalar(tcx, TypingEnv::fully_monomorphized())
        }))
        .ok()
        .flatten();

        if let Some(scalar) = scalar {
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
        let normalized_args = tcx
            .try_normalize_erasing_regions(typing_env, args)
            .unwrap_or(args);
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
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                continue;
            };
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
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                continue;
            };
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
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
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
        fn copy_source_local<'tcx>(
            pass: &MyOptimizationPass,
            body: &Body<'tcx>,
            local: Local,
            statements: &[Statement<'tcx>],
        ) -> Option<Local> {
            for stmt in statements.iter().rev() {
                let StatementKind::Assign(box (dst, rvalue)) = &stmt.kind else {
                    continue;
                };
                if dst.as_local() != Some(local) {
                    continue;
                }
                let src_local = match rvalue {
                    Rvalue::Use(op) => pass.place_from_operand(op).and_then(|p| p.as_local()),
                    Rvalue::CopyForDeref(p) => p.as_local(),
                    Rvalue::Ref(_, _, src) | Rvalue::RawPtr(_, src) => Some(src.local),
                    _ => None,
                }?;
                if pass.is_pointer_ty(body.local_decls[src_local].ty) {
                    return Some(src_local);
                }
                return None;
            }
            None
        }

        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(place_ty) {
            return None;
        }

        if place.projection.is_empty() {
            let local = place.local;
            if self.is_pointer_ty(body.local_decls[local].ty) {
                if let Some(src_local) =
                    copy_source_local(self, body, local, &block_data.statements)
                {
                    return Some(src_local);
                }
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

    /// For a pointer local `dst_local`, recover lineage from a same-block defining assignment
    /// whose RHS is a projected pointer place such as:
    ///   `_v = copy (((_agg as Some).0).1)`
    ///
    /// This is the shape seen in iterator-returned aggregates like `Option<(&K, &V)>`, where
    /// the extracted pointer local would otherwise remain untagged and later be rooted at a call
    /// boundary.
    fn recover_projected_pointer_rhs_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        dst_local: Local,
    ) -> Option<Local> {
        let block_stmts = &body.basic_blocks[bb].statements;
        let upto = stmt_idx.min(block_stmts.len());
        for stmt in block_stmts[..upto].iter().rev() {
            let StatementKind::Assign(box (dst, rvalue)) = &stmt.kind else {
                continue;
            };
            if dst.as_local() != Some(dst_local) {
                continue;
            }
            let src_place = match rvalue {
                Rvalue::Use(op) => self.place_from_operand(op),
                Rvalue::CopyForDeref(p) => Some(*p),
                Rvalue::Cast(
                    CastKind::PtrToPtr
                    | CastKind::PointerCoercion(_, _)
                    | CastKind::Transmute
                    | CastKind::PointerWithExposedProvenance,
                    op,
                    _,
                ) => self.place_from_operand(op),
                _ => None,
            }?;
            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
            if !self.is_pointer_ty(src_ty) || src_place.projection.is_empty() {
                return None;
            }
            return self.recover_pointer_source_local_for_projected_place(
                tcx, body, bb, upto, src_place, false,
            );
        }
        None
    }

    fn recover_projected_pointer_rhs_source<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        dst_local: Local,
    ) -> Option<(BasicBlock, usize, Place<'tcx>)> {
        let predecessors = body.basic_blocks.predecessors();
        let mut cur_bb = bb;
        let mut upto = stmt_idx.min(body.basic_blocks[cur_bb].statements.len());
        let mut visited: HashSet<BasicBlock> = HashSet::new();

        loop {
            let block_stmts = &body.basic_blocks[cur_bb].statements;
            for (def_stmt_idx, stmt) in block_stmts[..upto].iter().enumerate().rev() {
                let StatementKind::Assign(box (dst, rvalue)) = &stmt.kind else {
                    continue;
                };
                if dst.as_local() != Some(dst_local) {
                    continue;
                }
                let src_place = match rvalue {
                    Rvalue::Use(op) => self.place_from_operand(op),
                    Rvalue::CopyForDeref(p) => Some(*p),
                    Rvalue::Cast(
                        CastKind::PtrToPtr
                        | CastKind::PointerCoercion(_, _)
                        | CastKind::Transmute
                        | CastKind::PointerWithExposedProvenance,
                        op,
                        _,
                    ) => self.place_from_operand(op),
                    _ => None,
                }?;
                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(src_ty) && !src_place.projection.is_empty() {
                    return Some((cur_bb, def_stmt_idx, src_place));
                }
                return None;
            }

            let preds = &predecessors[cur_bb];
            if preds.len() != 1 {
                return None;
            }
            let pred_bb = preds[0];
            if !visited.insert(pred_bb) {
                return None;
            }
            cur_bb = pred_bb;
            upto = body.basic_blocks[cur_bb].statements.len();
        }
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
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
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

    /// Recover the pointee local for a mutable-reference local.
    ///
    /// Typical shape:
    ///   _tmp = &mut _p;
    ///   call(..., copy _tmp, ...);
    ///
    /// Returns `_p` for `_tmp`. We first walk backward through the current block to keep the
    /// common same-block case cheap. Optimized MIR can hoist the temp definition into a
    /// predecessor block, though:
    ///
    /// ```text
    /// bb0:
    ///   _tmp = &mut _p;
    ///   goto bb1;
    ///
    /// bb1:
    ///   call(..., move _tmp, ...);
    /// ```
    ///
    /// In that case the caller-side `MutArgRetTake` path still needs to recover `_p` so the
    /// returned family is written back into `_p`'s anchor after the call returns. Fall back to a
    /// whole-body unique-definition walk when the same-block walk misses.
    fn backtrack_mut_ref_pointee_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        ref_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        let mut current_local = ref_local;
        let mut search_end = statements.len();

        'outer: loop {
            for (idx, stmt) in statements[..search_end].iter().enumerate().rev() {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
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

            break;
        }

        self.backtrack_mut_ref_pointee_local_body(body, current_local)
    }

    fn backtrack_mut_ref_pointee_local_body<'tcx>(
        &self,
        body: &Body<'tcx>,
        ref_local: Local,
    ) -> Option<Local> {
        let mut current_local = ref_local;

        'outer: loop {
            let mut found_rvalue: Option<&Rvalue<'tcx>> = None;

            for block_data in body.basic_blocks.iter() {
                for stmt in &block_data.statements {
                    let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                        continue;
                    };
                    if place.as_local() != Some(current_local) {
                        continue;
                    }
                    if found_rvalue.is_some() {
                        return None;
                    }
                    found_rvalue = Some(rvalue);
                }
            }

            let rvalue = found_rvalue?;
            match rvalue {
                Rvalue::Ref(_, BorrowKind::Mut { .. }, src_place) => {
                    return Some(src_place.local);
                }
                Rvalue::Use(op) => {
                    let Some(next_local) = self.place_from_operand(op).and_then(|p| p.as_local())
                    else {
                        return None;
                    };
                    current_local = next_local;
                    continue 'outer;
                }
                Rvalue::CopyForDeref(p) => {
                    let Some(next_local) = p.as_local() else {
                        return None;
                    };
                    current_local = next_local;
                    continue 'outer;
                }
                Rvalue::Cast(
                    CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                    op,
                    _,
                )
                | Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                    let Some(next_local) = self.place_from_operand(op).and_then(|p| p.as_local())
                    else {
                        return None;
                    };
                    current_local = next_local;
                    continue 'outer;
                }
                _ => return None,
            }
        }
    }

    /// Recover the non-pointer pointee local behind a local that ultimately comes from
    /// `&mut _p`, `&_p`, or `&raw {_mut,const} _p`.
    ///
    /// We first walk backward within the current block. If the temporary feeding the pointer local
    /// was hoisted into a predecessor block, fall back to a whole-body unique-def walk so
    /// call-boundary recovery can still find the underlying stack local.
    fn backtrack_pointer_pointee_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        ptr_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        let mut current_local = ptr_local;
        let mut search_end = statements.len();

        'outer: loop {
            for (idx, stmt) in statements[..search_end].iter().enumerate().rev() {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
                if place.as_local() != Some(current_local) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(_, _, src_place) | Rvalue::RawPtr(_, src_place) => {
                        if !self.is_pointer_ty(body.local_decls[src_place.local].ty) {
                            return Some(src_place.local);
                        }
                        current_local = src_place.local;
                        search_end = idx;
                        continue 'outer;
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

            return self.backtrack_pointer_pointee_local_body(body, current_local);
        }
    }

    fn backtrack_pointer_pointee_local_body<'tcx>(
        &self,
        body: &Body<'tcx>,
        ptr_local: Local,
    ) -> Option<Local> {
        let mut current_local = ptr_local;

        loop {
            let mut found_rvalue: Option<&Rvalue<'tcx>> = None;
            for block in body.basic_blocks.iter() {
                for stmt in &block.statements {
                    let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                        continue;
                    };
                    if place.as_local() != Some(current_local) {
                        continue;
                    }
                    if found_rvalue.is_some() {
                        return None;
                    }
                    found_rvalue = Some(rvalue);
                }
            }

            let rvalue = found_rvalue?;
            match rvalue {
                Rvalue::Ref(_, _, src_place) | Rvalue::RawPtr(_, src_place) => {
                    if !self.is_pointer_ty(body.local_decls[src_place.local].ty) {
                        return Some(src_place.local);
                    }
                    current_local = src_place.local;
                }
                Rvalue::Use(op) => {
                    let next_local = self.place_from_operand(op).and_then(|p| p.as_local())?;
                    current_local = next_local;
                }
                Rvalue::CopyForDeref(p) => {
                    let next_local = p.as_local()?;
                    current_local = next_local;
                }
                Rvalue::Cast(
                    CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                    op,
                    _,
                )
                | Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                    let next_local = self.place_from_operand(op).and_then(|p| p.as_local())?;
                    current_local = next_local;
                }
                _ => return None,
            }
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
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
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

                let Some(next_local) = next_local else {
                    return None;
                };

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

    /// Whole-body fallback for wrapper loads where the immediate carrier local was defined in a
    /// predecessor block (for example `_box = deref_copy(*_slot_ptr); _raw = transmute(_box.0)`).
    ///
    /// We follow only simple forwarding/address-of shapes and require a unique source at every
    /// step. If the final unique local is pointer-typed, we return it.
    fn backtrack_global_pointer_value_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        dst_local: Local,
    ) -> Option<Local> {
        let mut current_local = dst_local;
        let mut visited: HashSet<Local> = HashSet::new();

        loop {
            if !visited.insert(current_local) {
                return None;
            }

            let mut matched = false;
            let mut recovered: Option<Local> = None;

            for block_data in body.basic_blocks.iter() {
                for stmt in &block_data.statements {
                    let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                        continue;
                    };
                    if place.as_local() != Some(current_local) {
                        continue;
                    }
                    matched = true;

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
                        Rvalue::CopyForDeref(p) => p.as_local().or_else(|| {
                            if self.is_pointer_ty(body.local_decls[p.local].ty) {
                                Some(p.local)
                            } else {
                                None
                            }
                        }),
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute,
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

                    let Some(next_local) = next_local else {
                        return None;
                    };

                    match recovered {
                        Some(existing) if existing != next_local => return None,
                        Some(_) => {}
                        None => recovered = Some(next_local),
                    }
                }
            }

            if !matched {
                return self
                    .is_pointer_ty(body.local_decls[current_local].ty)
                    .then_some(current_local);
            }

            let next_local = recovered?;
            current_local = next_local;
        }
    }

    /// Recover the original pointer local behind a reversible exposed-provenance round-trip.
    ///
    /// Target shape:
    /// - `ptr as usize` (`PointerExposeProvenance`)
    /// - simple forwarding / integer munging with one local input and const operands
    /// - `usize as *mut T` / `usize as *const T` (`PointerWithExposedProvenance`)
    ///
    /// This is intentionally narrow. It exists for patterns such as `bytes::ptr_map`, where the
    /// native build lowers pointer tagging to exposed-provenance integer casts, but the result is
    /// still derived from a real pointer input rather than forged from an arbitrary integer.
    fn backtrack_global_exposed_provenance_source_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        dst_local: Local,
    ) -> Option<Local> {
        enum ExposedProvDef<'a, 'tcx> {
            Rvalue(&'a Rvalue<'tcx>),
            CallArgs(&'a [Spanned<Operand<'tcx>>]),
        }

        fn one_local_one_const<'tcx>(
            pass: &MyOptimizationPass,
            body: &Body<'tcx>,
            lhs: &Operand<'tcx>,
            rhs: &Operand<'tcx>,
        ) -> Option<Local> {
            let lhs_local = pass.place_from_operand(lhs).and_then(|p| p.as_local());
            let rhs_local = pass.place_from_operand(rhs).and_then(|p| p.as_local());
            match (lhs_local, rhs_local) {
                (Some(local), None) | (None, Some(local))
                    if body.local_decls[local].ty.is_integral() =>
                {
                    Some(local)
                }
                _ => None,
            }
        }

        fn inner<'tcx>(
            pass: &MyOptimizationPass,
            body: &Body<'tcx>,
            current_local: Local,
            visited: &mut HashSet<Local>,
        ) -> Option<Local> {
            if !visited.insert(current_local) {
                return None;
            }

            let mut found: Option<ExposedProvDef<'_, 'tcx>> = None;

            for block_data in body.basic_blocks.iter() {
                for stmt in &block_data.statements {
                    let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                        continue;
                    };
                    if place.as_local() != Some(current_local) {
                        continue;
                    }
                    if found.is_some() {
                        return None;
                    }
                    found = Some(ExposedProvDef::Rvalue(rvalue));
                }

                if let Some(term) = &block_data.terminator {
                    if let TerminatorKind::Call {
                        args, destination, ..
                    } = &term.kind
                    {
                        if destination.as_local() == Some(current_local) {
                            if found.is_some() {
                                return None;
                            }
                            found = Some(ExposedProvDef::CallArgs(args));
                        }
                    }
                }
            }

            match found? {
                ExposedProvDef::Rvalue(rvalue) => match rvalue {
                    Rvalue::Use(op) => {
                        let next_local = pass.place_from_operand(op).and_then(|p| p.as_local())?;
                        inner(pass, body, next_local, visited)
                    }
                    Rvalue::CopyForDeref(place) => {
                        let next_local = place.as_local()?;
                        inner(pass, body, next_local, visited)
                    }
                    Rvalue::Cast(CastKind::PointerExposeProvenance, op, _) => {
                        let source_local =
                            pass.place_from_operand(op).and_then(|p| p.as_local())?;
                        pass.is_pointer_ty(body.local_decls[source_local].ty)
                            .then_some(source_local)
                    }
                    Rvalue::Cast(
                        CastKind::IntToInt
                        | CastKind::Transmute
                        | CastKind::PointerWithExposedProvenance,
                        op,
                        _,
                    ) => {
                        let next_local = pass.place_from_operand(op).and_then(|p| p.as_local())?;
                        inner(pass, body, next_local, visited)
                    }
                    Rvalue::BinaryOp(binop, box (lhs, rhs))
                        if matches!(
                            binop,
                            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Add | BinOp::Sub
                        ) =>
                    {
                        let next_local = one_local_one_const(pass, body, lhs, rhs)?;
                        inner(pass, body, next_local, visited)
                    }
                    Rvalue::Aggregate(_, ops) if ops.len() == 1 => {
                        let next_local = pass
                            .place_from_operand(ops.iter().next()?)
                            .and_then(|p| p.as_local())?;
                        inner(pass, body, next_local, visited)
                    }
                    _ => None,
                },
                ExposedProvDef::CallArgs(args) => {
                    let mut recovered: Option<Local> = None;
                    for arg in args.iter() {
                        let Some(arg_local) = pass
                            .place_from_operand(&arg.node)
                            .and_then(|p| p.as_local())
                        else {
                            continue;
                        };
                        let mut branch_visited = visited.clone();
                        let Some(candidate) = inner(pass, body, arg_local, &mut branch_visited)
                        else {
                            continue;
                        };
                        match recovered {
                            Some(existing) if existing != candidate => return None,
                            Some(_) => {}
                            None => recovered = Some(candidate),
                        }
                    }
                    recovered
                }
            }
        }

        let mut visited: HashSet<Local> = HashSet::new();
        inner(self, body, dst_local, &mut visited)
    }

    fn call_arg_push_needs_canonical_boundary_validate<'tcx>(
        &self,
        body: &Body<'tcx>,
        ptr_local: Local,
        boundary_recovered_ptr_locals: &HashSet<Local>,
    ) -> bool {
        if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::RawPtr(..)) {
            return true;
        }
        if !matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
            return false;
        }
        if boundary_recovered_ptr_locals.contains(&ptr_local) {
            return true;
        }
        self.backtrack_global_pointer_value_local(body, ptr_local)
            .is_some_and(|src_local| {
                src_local != ptr_local && boundary_recovered_ptr_locals.contains(&src_local)
            })
    }

    fn call_arg_push_flags_for_ptr_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        ptr_local: Local,
        exact_inplace_source: bool,
        boundary_recovered_ptr_locals: &HashSet<Local>,
    ) -> u8 {
        let mut flags = if exact_inplace_source {
            CALL_ARG_FLAG_INPLACE_EXACT_SOURCE
        } else {
            0
        };
        if self.call_arg_push_needs_canonical_boundary_validate(
            body,
            ptr_local,
            boundary_recovered_ptr_locals,
        ) {
            flags |= CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE;
        }
        if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..))
            && self.rhs_or_local_carries_boundary_recovered_ptr(
                body,
                ptr_local,
                boundary_recovered_ptr_locals,
            )
        {
            flags |= CALL_ARG_FLAG_USE_EXPORT_PARENT;
        }
        if matches!(
            body.local_decls[ptr_local].ty.kind(),
            TyKind::Ref(_, pointee_ty, Mutability::Mut)
                if !self.is_pointer_ty(*pointee_ty)
        ) {
            flags |= CALL_ARG_FLAG_USE_EXPORT_PARENT;
        }
        flags
    }

    fn local_is_direct_mut_ref_of_nonpointer_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        body.basic_blocks.iter().any(|block_data| {
            block_data.statements.iter().any(|stmt| {
                matches!(
                    &stmt.kind,
                    StatementKind::Assign(box (
                        lhs,
                        Rvalue::Ref(_, BorrowKind::Mut { .. }, src_place)
                    )) if lhs.as_local() == Some(local)
                        && src_place.projection.is_empty()
                        && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
                )
            })
        })
    }

    fn shadow_store_uses_local_slot_store<'tcx>(
        &self,
        local_slot_shadow_store_locals: &HashSet<Local>,
        place: Place<'tcx>,
        src_local: Local,
    ) -> bool {
        place.as_local() == Some(src_local)
            && place.projection.is_empty()
            && local_slot_shadow_store_locals.contains(&src_local)
    }

    fn rhs_or_local_carries_boundary_recovered_ptr<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
        boundary_recovered_ptr_locals: &HashSet<Local>,
    ) -> bool {
        boundary_recovered_ptr_locals.contains(&local)
            || self
                .backtrack_global_pointer_value_local(body, local)
                .is_some_and(|src_local| {
                    src_local != local && boundary_recovered_ptr_locals.contains(&src_local)
                })
    }

    fn rhs_carries_boundary_recovered_ptr<'tcx>(
        &self,
        body: &Body<'tcx>,
        rvalue: &Rvalue<'tcx>,
        boundary_recovered_ptr_locals: &HashSet<Local>,
    ) -> bool {
        let source_local_is_recovered = |local: Local| {
            boundary_recovered_ptr_locals.contains(&local)
                || self
                    .backtrack_global_pointer_value_local(body, local)
                    .is_some_and(|src_local| {
                        src_local != local && boundary_recovered_ptr_locals.contains(&src_local)
                    })
        };

        match rvalue {
            Rvalue::Use(op) => self
                .place_from_operand(op)
                .is_some_and(|src_place| source_local_is_recovered(src_place.local)),
            Rvalue::Ref(_, _, src_place) => source_local_is_recovered(src_place.local),
            Rvalue::CopyForDeref(src_place) | Rvalue::RawPtr(_, src_place) => {
                source_local_is_recovered(src_place.local)
            }
            Rvalue::Cast(
                CastKind::PtrToPtr
                | CastKind::PointerCoercion(_, _)
                | CastKind::Transmute
                | CastKind::PointerWithExposedProvenance,
                op,
                _,
            ) => self
                .place_from_operand(op)
                .is_some_and(|src_place| source_local_is_recovered(src_place.local)),
            Rvalue::Aggregate(_, ops) => ops.iter().any(|op| {
                self.place_from_operand(op)
                    .is_some_and(|src_place| source_local_is_recovered(src_place.local))
            }),
            Rvalue::BinaryOp(BinOp::Offset, ops) => self
                .place_from_operand(&ops.0)
                .is_some_and(|src_place| source_local_is_recovered(src_place.local)),
            _ => false,
        }
    }

    /// Best-effort whole-body fallback: if `agg_local` is a non-pointer aggregate local, recover
    /// the single pointer local consistently packed into it across all assignments in the body.
    ///
    /// This is used when local block backtracking cannot find the carrier origin near the current
    /// use site, but we still want to recover lineage for wrappers like `Option<&T>` or single-ref
    /// tuple/struct carriers instead of dropping to `parent=0`.
    fn backtrack_global_single_pointer_carrier_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        agg_local: Local,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;

        for block_data in body.basic_blocks.iter() {
            for stmt in &block_data.statements {
                let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                    continue;
                };
                if place.as_local() != Some(agg_local) {
                    continue;
                }

                let local = match rvalue {
                    Rvalue::Aggregate(_, ops) => {
                        let mut candidate: Option<Local> = None;
                        for op in ops.iter() {
                            let Some(src_local) = self
                                .place_from_operand(op)
                                .and_then(|p| p.as_local())
                                .filter(|local| self.is_pointer_ty(body.local_decls[*local].ty))
                            else {
                                continue;
                            };
                            match candidate {
                                Some(existing) if existing != src_local => return None,
                                Some(_) => {}
                                None => candidate = Some(src_local),
                            }
                        }
                        candidate
                    }
                    _ => return None,
                };

                let Some(local) = local else {
                    return None;
                };
                match recovered {
                    Some(existing) if existing != local => return None,
                    Some(_) => {}
                    None => recovered = Some(local),
                }
            }
        }

        recovered
    }

    /// Best-effort local-block backtracking for a non-pointer aggregate local that wraps exactly
    /// one pointer local.
    ///
    /// We scan recent assignments to `agg_local` and recover the unique pointer operand used to
    /// build carriers such as `Option<&T>`, `(&T, bool)`, or small wrapper structs. If multiple
    /// different pointer locals feed the aggregate, we return `None`.
    fn backtrack_single_pointer_carrier_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        agg_local: Local,
        statements: &[Statement<'tcx>],
    ) -> Option<Local> {
        for stmt in statements.iter().rev() {
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                continue;
            };
            if place.as_local() != Some(agg_local) {
                continue;
            }

            return match rvalue {
                Rvalue::Aggregate(_, ops) => {
                    let mut candidate: Option<Local> = None;
                    for op in ops.iter() {
                        let Some(src_local) = self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local())
                            .filter(|local| self.is_pointer_ty(body.local_decls[*local].ty))
                        else {
                            continue;
                        };
                        match candidate {
                            Some(existing) if existing != src_local => return None,
                            Some(_) => {}
                            None => candidate = Some(src_local),
                        }
                    }
                    candidate
                }
                _ => None,
            };
        }

        None
    }

    fn invalidate_ssa_anchors_for_local(
        &self,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        local: Local,
    ) {
        ssa_anchor_for_expr.retain(|_, state| !state.deps.contains(&local));
    }

    fn rebind_ssa_anchors_for_copy(
        &self,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        src: Local,
        dst: Local,
    ) {
        let rebound: Vec<(String, SsaAnchorState)> = ssa_anchor_for_expr
            .iter()
            .filter_map(|(key, state)| {
                if state.local == src {
                    Some((
                        key.clone(),
                        SsaAnchorState {
                            local: dst,
                            deps: state.deps.clone(),
                            source: state.source,
                        },
                    ))
                } else {
                    None
                }
            })
            .collect();
        for (key, state) in rebound {
            ssa_anchor_for_expr.insert(key, state);
        }
    }

    /// Return a reusable SSA anchor for a normalized pointer-expression key, if the existing
    /// anchor is type-compatible with `dst_local`.
    ///
    /// This lets repeated MIR expressions such as casts/copies/projected pointer computations
    /// reuse one previously-materialized tag/ref-ancestor source instead of synthesizing a fresh
    /// lineage chain every time.
    fn reusable_ssa_anchor_for_expr<'tcx>(
        &self,
        body: &Body<'tcx>,
        ssa_anchor_for_expr: &SsaAnchorMap,
        key: &str,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
    ) -> Option<SsaAnchorState> {
        let state = ssa_anchor_for_expr.get(key)?;
        if state.local == dst_local {
            return None;
        }
        match state.source {
            SsaAnchorSource::Tag => {
                if body.local_decls[state.local].ty != dst_ty {
                    return None;
                }
            }
            SsaAnchorSource::RefAncestor => {
                if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                    return None;
                }
            }
        }
        Some(state.clone())
    }

    /// Return a reusable SSA anchor for a normalized ref-source place key when the anchor local is
    /// itself pointer-typed.
    ///
    /// This is the ref-source-specific variant used for `&src` / `&mut src` style creations where
    /// we want to preserve the source pointer lineage instead of rebuilding it from scratch.
    fn reusable_ssa_anchor_for_ref_source_expr<'tcx>(
        &self,
        body: &Body<'tcx>,
        ssa_anchor_for_expr: &SsaAnchorMap,
        key: &str,
        dst_local: Local,
    ) -> Option<SsaAnchorState> {
        let state = ssa_anchor_for_expr.get(key)?;
        if state.local == dst_local {
            return None;
        }
        if !self.is_pointer_ty(body.local_decls[state.local].ty) {
            return None;
        }
        Some(state.clone())
    }

    /// Decide whether repeated ref creation from `src_place` may safely reuse a cached SSA anchor.
    ///
    /// We disable reuse in cases where reusing the last anchor would incorrectly turn independent
    /// derivations into a parent->child chain, notably raw-deref ref creation and TB shared-ref
    /// repetition.
    fn allow_ssa_anchor_reuse_for_ref_source_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        bk: BorrowKind,
        src_place: Place<'tcx>,
    ) -> bool {
        // Reusing the last ref-created anchor for `&*raw` / `&mut *raw` turns repeated
        // ref creation from the same raw pointer into a parent->child chain. For both SB-
        // and TB-style models these refs should derive from the raw pointer lineage instead.
        if matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && self.is_raw_pointer_ty(body.local_decls[src_place.local].ty)
        {
            return false;
        }

        // Reusing a prior ref local for `&mut local` where `local` is a whole non-pointer
        // aggregate turns the new mutable borrow into a child of the previous borrow of the
        // container itself. That is not pointer-lineage reuse; it is stale borrow-state reuse.
        if matches!(bk, BorrowKind::Mut { .. })
            && src_place.projection.is_empty()
            && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
        {
            return false;
        }

        // In TB mode, repeating `&_1` should not silently chain the second shared ref off the
        // first one; that collapses independent shared-to-raw derivations into one lineage and
        // hides later mutable conflicts.
        if !matches!(bk, BorrowKind::Mut { .. }) && !self.compile_alias_model_is_sb_like() {
            return false;
        }

        true
    }

    /// Drop SSA anchors that may no longer be valid across a call boundary.
    ///
    /// Any anchor depending on the call destination or argument locals is conservatively removed,
    /// except for some ref-ancestor anchors whose dependencies are only non-pointer carrier locals.
    fn invalidate_ssa_anchors_for_call<'tcx>(
        &self,
        body: &Body<'tcx>,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        trace_ssa_anchor: bool,
    ) {
        let mut touched_locals: HashSet<Local> = HashSet::new();
        if let Some(dst_local) = destination.as_local() {
            touched_locals.insert(dst_local);
        }
        for arg in args.iter() {
            if let Some(place) = self.place_from_operand(&arg.node) {
                touched_locals.insert(place.local);
            }
        }
        let before = ssa_anchor_for_expr.len();
        ssa_anchor_for_expr.retain(|_, state| {
            if state.deps.iter().all(|dep| !touched_locals.contains(dep)) {
                return true;
            }
            matches!(state.source, SsaAnchorSource::RefAncestor)
                && !state.deps.is_empty()
                && state.deps.iter().all(|dep| {
                    touched_locals.contains(dep) && !self.is_pointer_ty(body.local_decls[*dep].ty)
                })
        });
        if trace_ssa_anchor
            && before != ssa_anchor_for_expr.len()
            && self.log_enabled(PassLogLevel::Trace)
        {
            rz_pass_trace!(
                self,
                "[rusteze][ssa-anchor] invalidate-call touched={:?} kept={} dropped={}",
                touched_locals,
                ssa_anchor_for_expr.len(),
                before.saturating_sub(ssa_anchor_for_expr.len()),
            );
        }
    }

    fn meet_ssa_anchor_maps<'a>(
        &self,
        pred_maps: impl IntoIterator<Item = &'a SsaAnchorMap>,
    ) -> SsaAnchorMap {
        let pred_maps: Vec<&SsaAnchorMap> = pred_maps.into_iter().collect();
        let Some(first) = pred_maps.first() else {
            return HashMap::new();
        };
        let mut merged = (*first).clone();
        merged.retain(|key, value| pred_maps.iter().all(|map| map.get(key) == Some(value)));
        merged
    }

    /// Forward dataflow analysis computing the SSA-anchor map available at entry to each basic
    /// block.
    ///
    /// The pass simulates `scan_statement` effects, meets predecessor maps, and applies call-site
    /// invalidation so later instrumentation can reuse stable anchors across CFG joins.
    fn analyze_ssa_anchor_entry_maps<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
        summary_elidable_shared_call_ref_locals: &HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
    ) -> HashMap<BasicBlock, SsaAnchorMap> {
        let predecessors = body.basic_blocks.predecessors();
        let mut entry_by_bb: HashMap<BasicBlock, SsaAnchorMap> = HashMap::new();
        let mut exit_by_bb: HashMap<BasicBlock, SsaAnchorMap> = HashMap::new();
        let mut worklist: VecDeque<BasicBlock> =
            traversal::preorder(body).map(|(bb, _)| bb).collect();
        let mut queued: HashSet<BasicBlock> = worklist.iter().copied().collect();

        while let Some(bb) = worklist.pop_front() {
            queued.remove(&bb);
            let block_data = &body.basic_blocks[bb];

            let entry = if predecessors[bb].is_empty() {
                HashMap::new()
            } else {
                self.meet_ssa_anchor_maps(
                    predecessors[bb]
                        .iter()
                        .filter_map(|pred| exit_by_bb.get(pred)),
                )
            };

            let mut exit = entry.clone();
            let mut dummy_insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
            let mut byte_copy_src_for_local: HashMap<Local, Place<'tcx>> = HashMap::new();
            let mut dummy_projected_reborrow_anchor_specs: ReborrowAnchorSpecMap = HashMap::new();
            let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();
            let mut tagged_ptr_locals: HashSet<Local> = HashSet::new();
            let mut boundary_recovered_ptr_locals: HashSet<Local> = HashSet::new();

            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                self.scan_statement(
                    tcx,
                    body,
                    bb,
                    block_data,
                    stmt_idx,
                    stmt,
                    &mut dummy_insert_points,
                    &mut byte_copy_src_for_local,
                    &mut exit,
                    &mut dummy_projected_reborrow_anchor_specs,
                    &mut ptr_locals_needing_tag,
                    &mut tagged_ptr_locals,
                    &mut boundary_recovered_ptr_locals,
                    ptr_locals_with_tag_sources,
                    summary_elidable_shared_call_ref_locals,
                    interesting_stack_locals,
                    track_all_stack_allocs,
                    false,
                );
            }

            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call {
                    args, destination, ..
                } = &term.kind
                {
                    self.invalidate_ssa_anchors_for_call(body, &mut exit, args, destination, false);
                }
            }

            let entry_changed = entry_by_bb.get(&bb) != Some(&entry);
            let exit_changed = exit_by_bb.get(&bb) != Some(&exit);
            if entry_changed {
                entry_by_bb.insert(bb, entry);
            }
            if exit_changed {
                exit_by_bb.insert(bb, exit);
                if let Some(term) = &block_data.terminator {
                    for succ in term.successors() {
                        if queued.insert(succ) {
                            worklist.push_back(succ);
                        }
                    }
                }
            }
        }

        entry_by_bb
    }

    fn normalized_ptr_expr_key_for_rvalue<'tcx>(
        &self,
        body: &Body<'tcx>,
        rvalue: &Rvalue<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
    ) -> Option<(String, Vec<Local>)> {
        let mut visited: HashSet<Local> = HashSet::new();
        let mut deps: HashSet<Local> = HashSet::new();
        let key = self.normalized_ptr_rvalue_key(
            body,
            rvalue,
            statements,
            upto,
            16,
            &mut visited,
            &mut deps,
        )?;
        let mut deps_vec: Vec<Local> = deps.into_iter().collect();
        deps_vec.sort_by_key(|local| local.index());
        Some((key, deps_vec))
    }

    fn normalized_ptr_expr_key_for_ref_source_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
    ) -> Option<(String, Vec<Local>)> {
        let mut visited: HashSet<Local> = HashSet::new();
        let mut deps: HashSet<Local> = HashSet::new();
        let key = self.normalized_ptr_place_key(
            body,
            src_place,
            statements,
            upto,
            16,
            &mut visited,
            &mut deps,
        )?;
        let mut deps_vec: Vec<Local> = deps.into_iter().collect();
        deps_vec.sort_by_key(|local| local.index());
        Some((key, deps_vec))
    }

    fn projected_reborrow_anchor_dep_ok<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        !matches!(ty.kind(), TyKind::RawPtr(..))
    }

    fn projected_reborrow_anchor_eligible<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        deps: &[Local],
    ) -> bool {
        if src_place.projection.is_empty() {
            return false;
        }

        let base_ty = body.local_decls[src_place.local].ty;
        let no_deref_projection =
            !self.place_contains_deref(src_place) && !self.is_pointer_ty(base_ty);
        let leading_ref_deref_projection = matches!(base_ty.kind(), TyKind::Ref(..))
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place
                .projection
                .iter()
                .skip(1)
                .all(|pe| !matches!(pe, ProjectionElem::Deref));

        if !(no_deref_projection || leading_ref_deref_projection) {
            return false;
        }

        deps.iter()
            .all(|dep| self.projected_reborrow_anchor_dep_ok(body.local_decls[*dep].ty))
    }

    fn maybe_projected_reborrow_anchor_key<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        anchor_key: Option<&(String, Vec<Local>)>,
        projected_reborrow_anchor_specs: &mut ReborrowAnchorSpecMap,
    ) -> Option<String> {
        if matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::ReceiverFamily
        ) {
            return None;
        }
        let (key, deps) = anchor_key?;
        if !self.projected_reborrow_anchor_eligible(body, src_place, deps) {
            return None;
        }
        projected_reborrow_anchor_specs
            .entry(key.clone())
            .or_insert_with(|| deps.clone());
        Some(key.clone())
    }

    fn projected_reborrow_anchor_allowed_for_src_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> bool {
        matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::PointeeFamily
        )
    }

    fn normalized_ptr_rvalue_key<'tcx>(
        &self,
        body: &Body<'tcx>,
        rvalue: &Rvalue<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
        fuel: usize,
        visited: &mut HashSet<Local>,
        deps: &mut HashSet<Local>,
    ) -> Option<String> {
        if fuel == 0 {
            return None;
        }

        match rvalue {
            Rvalue::Use(op) => {
                let op_key = self.normalized_ptr_operand_key(
                    body,
                    op,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("use({op_key})"))
            }
            Rvalue::CopyForDeref(place) => {
                let place_key = self.normalized_ptr_place_key(
                    body,
                    *place,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("copyderef({place_key})"))
            }
            Rvalue::Cast(
                CastKind::PtrToPtr
                | CastKind::PointerCoercion(_, _)
                | CastKind::Transmute
                | CastKind::PointerWithExposedProvenance,
                op,
                _,
            ) => {
                let op_key = self.normalized_ptr_operand_key(
                    body,
                    op,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("cast({op_key})"))
            }
            Rvalue::BinaryOp(op, box (lhs, rhs))
                if matches!(*op, BinOp::Offset | BinOp::Add | BinOp::Sub) =>
            {
                let lhs_key = self.normalized_ptr_operand_key(
                    body,
                    lhs,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                let rhs_key = self.normalized_ptr_operand_key(
                    body,
                    rhs,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("binop({op:?},{lhs_key},{rhs_key})"))
            }
            Rvalue::Ref(_, _, src_place) => {
                let place_key = self.normalized_ptr_place_key(
                    body,
                    *src_place,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("ref({place_key})"))
            }
            Rvalue::RawPtr(_, src_place) => {
                let place_key = self.normalized_ptr_place_key(
                    body,
                    *src_place,
                    statements,
                    upto,
                    fuel - 1,
                    visited,
                    deps,
                )?;
                Some(format!("raw({place_key})"))
            }
            Rvalue::Aggregate(_, ops) => {
                let mut parts = Vec::new();
                for op in ops.iter() {
                    parts.push(self.normalized_ptr_operand_key(
                        body,
                        op,
                        statements,
                        upto,
                        fuel - 1,
                        visited,
                        deps,
                    )?);
                }
                Some(format!("agg({})", parts.join(",")))
            }
            _ => None,
        }
    }

    fn normalized_ptr_operand_key<'tcx>(
        &self,
        body: &Body<'tcx>,
        operand: &Operand<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
        fuel: usize,
        visited: &mut HashSet<Local>,
        deps: &mut HashSet<Local>,
    ) -> Option<String> {
        if fuel == 0 {
            return None;
        }

        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.normalized_ptr_place_key(
                body,
                *place,
                statements,
                upto,
                fuel - 1,
                visited,
                deps,
            ),
            Operand::Constant(c) => Some(format!("const({:?})", c.const_)),
        }
    }

    fn normalized_ptr_place_key<'tcx>(
        &self,
        body: &Body<'tcx>,
        place: Place<'tcx>,
        statements: &[Statement<'tcx>],
        upto: usize,
        fuel: usize,
        visited: &mut HashSet<Local>,
        deps: &mut HashSet<Local>,
    ) -> Option<String> {
        if fuel == 0 {
            return None;
        }

        let base_key = self.normalized_ptr_local_key(
            body,
            place.local,
            statements,
            upto,
            fuel - 1,
            visited,
            deps,
        )?;

        if place.projection.is_empty() {
            Some(base_key)
        } else {
            Some(format!("{base_key}{:?}", place.projection))
        }
    }

    fn normalized_ptr_local_key<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
        statements: &[Statement<'tcx>],
        upto: usize,
        fuel: usize,
        visited: &mut HashSet<Local>,
        deps: &mut HashSet<Local>,
    ) -> Option<String> {
        if fuel == 0 {
            return None;
        }
        if !visited.insert(local) {
            return None;
        }

        for (idx, stmt) in statements[..upto].iter().enumerate().rev() {
            let StatementKind::Assign(box (place, rvalue)) = &stmt.kind else {
                continue;
            };
            if place.as_local() != Some(local) {
                continue;
            }

            let result = self.normalized_ptr_rvalue_key(
                body,
                rvalue,
                statements,
                idx,
                fuel - 1,
                visited,
                deps,
            );
            visited.remove(&local);
            return result;
        }

        deps.insert(local);
        visited.remove(&local);
        Some(format!("L{}", local.index()))
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
                    && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
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
            candidate_local =
                self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto]);
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
        let projected_carrier_raw_field_load =
            self.is_projected_carrier_raw_field_load(body, src_place, is_raw_creation);
        let projected_raw_field_load = self.is_projected_raw_field_load(src_place, is_raw_creation);
        let mut src_local_opt = self.recover_parent_source_local_for_place(
            tcx,
            body,
            bb,
            stmt_idx,
            src_place,
            is_raw_creation,
        );

        if src_local_opt.is_none()
            && !projected_carrier_raw_field_load
            && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
        {
            src_local_opt = self.backtrack_single_pointer_arg_call_result_source_local(
                tcx,
                body,
                bb,
                src_place.local,
            );
            if src_local_opt.is_none() {
                src_local_opt = self.backtrack_global_pointer_arg_call_result_source_local(
                    tcx,
                    body,
                    src_place.local,
                );
            }
        }

        if src_local_opt.is_none()
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place
                .projection
                .iter()
                .skip(1)
                .any(|pe| matches!(pe, ProjectionElem::Field(_, _)))
            && self.is_pointer_ty(body.local_decls[src_place.local].ty)
            && !projected_raw_field_load
        {
            src_local_opt = Some(src_place.local);
        }

        if src_local_opt.is_none()
            && !src_place.projection.is_empty()
            && matches!(src_place.projection[0], ProjectionElem::Deref)
            && !projected_raw_field_load
        {
            if let Some(backtracked_local) =
                self.backtrack_deref_base_local(src_place.local, &block_data.statements[..stmt_idx])
            {
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

    fn receiver_family_base_local_for_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> Option<Local> {
        if src_place.projection.is_empty() {
            return None;
        }

        let base_local = src_place.local;
        let base_ty = body.local_decls[base_local].ty;
        let mut projection = src_place.projection.as_ref();

        if matches!(projection.first(), Some(ProjectionElem::Deref)) {
            let pointee_ty = match base_ty.kind() {
                TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
                _ => return None,
            };
            if self.is_pointer_ty(pointee_ty) {
                return None;
            }
            projection = &projection[1..];
        } else if self.is_pointer_ty(base_ty) {
            return None;
        }

        if projection.is_empty()
            || projection
                .iter()
                .any(|pe| matches!(pe, ProjectionElem::Deref))
        {
            return None;
        }

        if !projection.iter().any(|pe| {
            matches!(
                pe,
                ProjectionElem::Field(_, _)
                    | ProjectionElem::Downcast(..)
                    | ProjectionElem::OpaqueCast(_)
            )
        }) {
            return None;
        }

        Some(base_local)
    }

    fn receiver_family_parent_operand_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        source_info: SourceInfo,
        src_place: Place<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<Operand<'tcx>> {
        let base_local = self.receiver_family_base_local_for_place(body, src_place)?;

        if let Some(op) = self.exact_or_ref_ancestor_parent_operand_for_local(
            base_local,
            tag_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
        ) {
            return Some(op);
        }
        if let Some(op) = self.slot_family_parent_operand_for_local(
            base_local,
            reborrow_anchor_local_for_stack_local,
        ) {
            return Some(op);
        }

        let _ = (tcx, source_info);
        None
    }

    fn exact_or_ref_ancestor_parent_operand_for_local<'tcx>(
        &self,
        local: Local,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
    ) -> Option<Operand<'tcx>> {
        tag_local_for_ptr_local
            .get(&local)
            .copied()
            .map(|tag_local| Operand::Copy(Place::from(tag_local)))
            .or_else(|| {
                ref_ancestor_local_for_ptr_local
                    .get(&local)
                    .copied()
                    .map(|tag_local| Operand::Copy(Place::from(tag_local)))
            })
    }

    fn slot_family_parent_operand_for_local<'tcx>(
        &self,
        local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<Operand<'tcx>> {
        reborrow_anchor_local_for_stack_local
            .get(&local)
            .copied()
            .map(|anchor_local| Operand::Copy(Place::from(anchor_local)))
    }

    fn is_projected_carrier_raw_field_load<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
    ) -> bool {
        is_raw_creation
            && !src_place.projection.is_empty()
            && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
    }

    fn is_projected_raw_field_load(
        &self,
        src_place: Place<'_>,
        is_raw_creation: bool,
    ) -> bool {
        is_raw_creation
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place
                .projection
                .iter()
                .skip(1)
                .any(|pe| matches!(pe, ProjectionElem::Field(_, _)))
    }

    fn projected_slot_family_parent_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        src_place: Place<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        is_raw_creation: bool,
    ) -> Option<Operand<'tcx>> {
        if src_place.projection.is_empty() {
            return None;
        }

        let base_local = src_place.local;
        let base_ty = body.local_decls[base_local].ty;
        let projected_raw_field_load = self.is_projected_raw_field_load(src_place, is_raw_creation);

        if self.is_pointer_ty(base_ty)
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && !projected_raw_field_load
        {
            let block_stmts = &body.basic_blocks[bb].statements;
            let upto = stmt_idx.min(block_stmts.len());
            if let Some(pointee_local) =
                self.backtrack_pointer_pointee_local(body, base_local, &block_stmts[..upto])
            {
                if !self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                    if let Some(op) = self.slot_family_parent_operand_for_local(
                        pointee_local,
                        reborrow_anchor_local_for_stack_local,
                    ) {
                        return Some(op);
                    }
                }
            }
        }

        if !is_raw_creation
            && !self.is_pointer_ty(base_ty)
            && self.ty_contains_pointer_fields(tcx, body, base_ty, 4)
        {
            return self.slot_family_parent_operand_for_local(
                base_local,
                reborrow_anchor_local_for_stack_local,
            );
        }

        None
    }

    fn projected_fast_path_pointee_parent_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        is_raw_creation: bool,
    ) -> Option<Operand<'tcx>> {
        if src_place.projection.is_empty() {
            return None;
        }

        let base_local = src_place.local;
        let base_ty = body.local_decls[base_local].ty;
        let projected_raw_field_load = self.is_projected_raw_field_load(src_place, is_raw_creation);

        if self.is_pointer_ty(base_ty)
            && !self.is_thin_ptr_ty(tcx, body, base_ty)
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place.projection.iter().skip(1).all(|pe| {
                matches!(
                    pe,
                    ProjectionElem::Index(_)
                        | ProjectionElem::ConstantIndex { .. }
                        | ProjectionElem::Subslice { .. }
                        | ProjectionElem::OpaqueCast(_)
                )
            })
        {
            return self.exact_or_ref_ancestor_parent_operand_for_local(
                base_local,
                tag_local_for_ptr_local,
                ref_ancestor_local_for_ptr_local,
            );
        }

        if self.is_pointer_ty(base_ty)
            && src_place.projection.len() == 1
            && matches!(src_place.projection[0], ProjectionElem::Deref)
        {
            return self.exact_or_ref_ancestor_parent_operand_for_local(
                base_local,
                tag_local_for_ptr_local,
                ref_ancestor_local_for_ptr_local,
            );
        }

        if self.is_pointer_ty(base_ty)
            && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
            && src_place
                .projection
                .iter()
                .skip(1)
                .any(|pe| matches!(pe, ProjectionElem::Field(_, _)))
            && !projected_raw_field_load
        {
            return self.exact_or_ref_ancestor_parent_operand_for_local(
                base_local,
                tag_local_for_ptr_local,
                ref_ancestor_local_for_ptr_local,
            );
        }

        None
    }

    fn candidate_parent_source_local_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
        projectionless_raw_direct_pointer_carrier: bool,
    ) -> Option<Local> {
        let candidate_local = if src_place.projection.is_empty() {
            let block_stmts = &body.basic_blocks[bb].statements;
            let upto = stmt_idx.min(block_stmts.len());
            let src_local = src_place.local;
            let src_ty = body.local_decls[src_local].ty;
            if self.is_pointer_ty(src_ty) {
                if self.is_projected_raw_field_load(src_place, is_raw_creation) {
                    self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto])
                } else {
                    Some(src_local)
                }
            } else if self.ty_contains_direct_pointer_fields(tcx, body, src_ty) {
                None
            } else {
                self.backtrack_pointer_source_local(body, src_local, &block_stmts[..upto])
                    .or_else(|| {
                        self.backtrack_single_pointer_carrier_local(
                            body,
                            src_local,
                            &block_stmts[..upto],
                        )
                    })
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

        candidate_local.or_else(|| {
            if src_place.projection.is_empty()
                && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
                && !projectionless_raw_direct_pointer_carrier
            {
                self.backtrack_global_single_pointer_carrier_local(body, src_place.local)
            } else {
                None
            }
        })
    }

    fn pointee_family_parent_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        src_place: Place<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        is_raw_creation: bool,
        projectionless_raw_direct_pointer_carrier: bool,
    ) -> Option<Operand<'tcx>> {
        let candidate_local = self.candidate_parent_source_local_for_src_place(
            tcx,
            body,
            bb,
            stmt_idx,
            src_place,
            is_raw_creation,
            projectionless_raw_direct_pointer_carrier,
        )?;

        self.exact_or_ref_ancestor_parent_operand_for_local(
            candidate_local,
            tag_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
        )
    }

    fn parent_selection_mode_for_src_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> ParentSelectionMode {
        if self
            .receiver_family_base_local_for_place(body, src_place)
            .is_some()
        {
            ParentSelectionMode::ReceiverFamily
        } else {
            ParentSelectionMode::PointeeFamily
        }
    }

    fn creation_parent_selection_mode_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        use_projectionless_anchor: bool,
    ) -> ParentSelectionMode {
        if matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::ReceiverFamily
        ) {
            return ParentSelectionMode::ReceiverFamily;
        }

        if use_projectionless_anchor
            && src_place.projection.is_empty()
            && self.supports_slot_family_local(tcx, body, src_place.local)
        {
            return ParentSelectionMode::SlotFamily;
        }

        ParentSelectionMode::PointeeFamily
    }

    /// Choose the parent-family operand for a new ref/raw creation from `src_place`.
    ///
    /// For plain pointers this prefers the source local's concrete tag. For projected accesses
    /// through non-pointer carrier locals it falls back to the hidden anchor local when direct
    /// pointer-source recovery is not available.
    ///
    /// Whole-place refs such as `&mut other` for a local `BytesMut` must *not* inherit that
    /// carrier anchor: the anchor tracks the nested pointer family, while the new ref itself is a
    /// borrow of the destination stack slot. Reusing the imported carrier family here cross-parents
    /// one stack slot from another after by-value returns such as `other = self.shallow_clone()`.
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
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        is_raw_creation: bool,
        use_projectionless_anchor: bool,
        mode: ParentSelectionMode,
    ) -> Operand<'tcx> {
        let _ = (projectionless_anchor_suppressed_locals, use_projectionless_anchor);
        let src_local_ty = body.local_decls[src_place.local].ty;
        let projectionless_raw_direct_pointer_carrier = is_raw_creation
            && src_place.projection.is_empty()
            && !self.is_pointer_ty(src_local_ty)
            && self.ty_contains_direct_pointer_fields(tcx, body, src_local_ty);
        let receiver_parent = if matches!(mode, ParentSelectionMode::ReceiverFamily) {
            self.receiver_family_parent_operand_for_place(
                tcx,
                body,
                source_info,
                src_place,
                tag_local_for_ptr_local,
                ref_ancestor_local_for_ptr_local,
                reborrow_anchor_local_for_stack_local,
            )
        } else {
            None
        };
        let slot_parent = if matches!(mode, ParentSelectionMode::SlotFamily) {
            self.slot_family_parent_operand_for_local(
                src_place.local,
                reborrow_anchor_local_for_stack_local,
            )
        } else {
            None
        };

        receiver_parent
            .or(slot_parent)
            .or_else(|| {
                self.projected_fast_path_pointee_parent_operand_for_src_place(
                    tcx,
                    body,
                    src_place,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    is_raw_creation,
                )
            })
            .or_else(|| {
                self.projected_slot_family_parent_operand_for_src_place(
                    tcx,
                    body,
                    bb,
                    stmt_idx,
                    src_place,
                    reborrow_anchor_local_for_stack_local,
                    is_raw_creation,
                )
            })
            .or_else(|| {
                self.pointee_family_parent_operand_for_src_place(
                    tcx,
                    body,
                    bb,
                    stmt_idx,
                    src_place,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    is_raw_creation,
                    projectionless_raw_direct_pointer_carrier,
                )
            })
            .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0))
    }

    fn materialize_projected_reborrow_parent_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        src_place: Place<'tcx>,
        projected_reborrow_anchor_key: Option<&String>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        extra_stmts: &mut Vec<Statement<'tcx>>,
    ) -> Option<Local> {
        if matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::ReceiverFamily
        ) {
            // Projected reborrow anchors preserve nested pointee lineage across wrapper-heavy
            // projected accesses. Receiver-family reborrows such as `&(*self_ref)` or
            // `&(*bytes_ref).field` must stay under the current receiver family instead.
            // Reusing a nonzero projected anchor here lets nested payload lineage override the
            // receiver tag and later validates stack-carrier field reads with heap/sentinel
            // metadata.
            return None;
        }

        let anchor_local = projected_reborrow_anchor_key
            .and_then(|key| projected_reborrow_anchor_local_for_key.get(key))
            .copied()?;
        let fallback_parent = self.parent_tag_operand_for_src_place(
            tcx,
            body,
            bb,
            stmt_idx,
            source_info,
            src_place,
            tag_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
            reborrow_anchor_local_for_stack_local,
            projectionless_anchor_suppressed_locals,
            false,
            true,
            self.parent_selection_mode_for_src_place(body, src_place),
        );

        let fallback_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_is_zero_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.bool, source_info.span));
        let anchor_is_zero_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_is_set_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let fallback_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let selected_parent_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));

        extra_stmts.extend([
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(fallback_local),
                    Rvalue::Use(fallback_parent),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_is_zero_local),
                    Rvalue::BinaryOp(
                        BinOp::Eq,
                        Box::new((
                            Operand::Copy(Place::from(anchor_local)),
                            self.const_u64(tcx, source_info.span, 0),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_is_zero_u64_local),
                    Rvalue::Cast(
                        CastKind::IntToInt,
                        Operand::Copy(Place::from(anchor_is_zero_local)),
                        tcx.types.u64,
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_is_set_u64_local),
                    Rvalue::BinaryOp(
                        BinOp::Sub,
                        Box::new((
                            self.const_u64(tcx, source_info.span, 1),
                            Operand::Copy(Place::from(anchor_is_zero_u64_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_part_local),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Box::new((
                            Operand::Copy(Place::from(anchor_local)),
                            Operand::Copy(Place::from(anchor_is_set_u64_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(fallback_part_local),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Box::new((
                            Operand::Copy(Place::from(fallback_local)),
                            Operand::Copy(Place::from(anchor_is_zero_u64_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(selected_parent_local),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Box::new((
                            Operand::Copy(Place::from(anchor_part_local)),
                            Operand::Copy(Place::from(fallback_part_local)),
                        )),
                    ),
                ))),
            ),
        ]);

        Some(selected_parent_local)
    }

    fn materialize_projectionless_slot_anchor_parent_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        src_place: Place<'tcx>,
        borrow_kind: BorrowKind,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        extra_stmts: &mut Vec<Statement<'tcx>>,
    ) -> Option<Local> {
        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
        if !src_place.projection.is_empty()
            || self.is_pointer_ty(src_ty)
            || !projectionless_anchor_suppressed_locals.contains(&src_place.local)
        {
            return None;
        }

        let anchor_local = reborrow_anchor_local_for_stack_local
            .get(&src_place.local)
            .copied()?;
        let anchor_state_local = anchor_is_slot_family_local_for_stack_local
            .get(&src_place.local)
            .copied()?;

        let fallback_parent = self.parent_tag_operand_for_src_place(
            tcx,
            body,
            bb,
            stmt_idx,
            source_info,
            src_place,
            tag_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
            reborrow_anchor_local_for_stack_local,
            projectionless_anchor_suppressed_locals,
            false,
            matches!(borrow_kind, BorrowKind::Mut { .. })
                || self.compile_alias_model_is_sb_like()
                || !self.is_pointer_ty(src_ty),
            self.parent_selection_mode_for_src_place(body, src_place),
        );

        let fallback_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_state_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_state_not_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let fallback_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let anchor_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let selected_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));

        extra_stmts.extend([
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(fallback_local),
                    Rvalue::Use(fallback_parent),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_state_u64_local),
                    Rvalue::Cast(
                        CastKind::IntToInt,
                        Operand::Copy(Place::from(anchor_state_local)),
                        tcx.types.u64,
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_state_not_u64_local),
                    Rvalue::BinaryOp(
                        BinOp::Sub,
                        Box::new((
                            self.const_u64(tcx, source_info.span, 1),
                            Operand::Copy(Place::from(anchor_state_u64_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(fallback_part_local),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Box::new((
                            Operand::Copy(Place::from(anchor_state_not_u64_local)),
                            Operand::Copy(Place::from(fallback_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(anchor_part_local),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Box::new((
                            Operand::Copy(Place::from(anchor_state_u64_local)),
                            Operand::Copy(Place::from(anchor_local)),
                        )),
                    ),
                ))),
            ),
            Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(selected_local),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Box::new((
                            Operand::Copy(Place::from(fallback_part_local)),
                            Operand::Copy(Place::from(anchor_part_local)),
                        )),
                    ),
                ))),
            ),
        ]);

        Some(selected_local)
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
        for arg_local in body.args_iter() {
            if !self.is_pointer_ty(body.local_decls[arg_local].ty) {
                interesting.insert(arg_local);
            }
        }
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
                    TerminatorKind::Call {
                        args, destination, ..
                    } => {
                        let dst_local = destination.local;
                        if !self.is_pointer_ty(body.local_decls[dst_local].ty) {
                            let mut recovered_src: Option<Local> = None;
                            let mut ambiguous = false;
                            for arg_index in 0..args.len() {
                                if let Some(src_local) = self.call_arg_lineage_source_local(
                                    tcx, body, block_data, args, arg_index,
                                ) {
                                    match recovered_src {
                                        Some(existing) if existing != src_local => {
                                            ambiguous = true;
                                            break;
                                        }
                                        Some(_) => {}
                                        None => recovered_src = Some(src_local),
                                    }
                                }
                            }
                            if !ambiguous && recovered_src.is_some() {
                                interesting.insert(dst_local);
                            }
                        }
                    }
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

        // Carry exact-place anchors across plain local moves/copies of non-pointer carriers.
        // This is the compiler-side replacement for runtime same-address repair in shapes like:
        //   _2 = into_iter(move _1);
        //   _11 = move _9;
        //   _13 = &mut _11;
        // Once the source local is interesting, the move/copy destination must also keep an
        // anchor local so later borrows stay in the same family instead of rooting at parent=0.
        let mut changed = true;
        while changed {
            changed = false;
            for (_bb, block_data) in body.basic_blocks.iter_enumerated() {
                for stmt in block_data.statements.iter() {
                    let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                        continue;
                    };
                    let Some(dst_local) = dst_place.as_local() else {
                        continue;
                    };
                    if self.is_pointer_ty(body.local_decls[dst_local].ty) {
                        continue;
                    }
                    let mut derived_from_projected_ptr = false;
                    let src_local = match rvalue {
                        Rvalue::Use(Operand::Copy(src_place))
                        | Rvalue::Use(Operand::Move(src_place)) => {
                            if src_place.projection.is_empty() {
                                Some(src_place.local)
                            } else if src_place.ty(&body.local_decls, tcx).ty
                                == body.local_decls[dst_local].ty
                                && self.is_pointer_ty(body.local_decls[src_place.local].ty)
                                && matches!(
                                    src_place.projection.first(),
                                    Some(ProjectionElem::Deref)
                                )
                            {
                                derived_from_projected_ptr = true;
                                Some(src_place.local)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(src_local) = src_local {
                        if (interesting.contains(&src_local) || derived_from_projected_ptr)
                            && interesting.insert(dst_local)
                        {
                            changed = true;
                        }
                    }
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
        local_slot_shadow_store_locals: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        projectionless_anchor_suppressed_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
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
                if self.is_shadowable_ptr_ty(tcx, body, arg_ty) {
                    if matches!(arg_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
                        local_slot_shadow_store_locals.insert(arg_local);
                    }
                    insert_points.push(InsertPoint {
                        bb: entry_bb,
                        stmt_idx: entry_stmt_idx,
                        insert_before: false,
                        source_info: entry_source_info,
                        place: Place::from(arg_local),
                        kind: InstrKind::ShadowStore {
                            src_local: arg_local,
                        },
                    });
                }
            } else if self.supports_call_boundary_anchor_local(tcx, body, arg_local) {
                projectionless_anchor_suppressed_locals.insert(arg_local);
                insert_points.push(InsertPoint {
                    bb: entry_bb,
                    stmt_idx: entry_stmt_idx,
                    insert_before: false,
                    source_info: entry_source_info,
                    place: Place::from(arg_local),
                    kind: InstrKind::ArgAnchorTake {
                        callee_id,
                        arg_index: arg_index as u64,
                        local: arg_local,
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
        byte_copy_src_for_local: &mut HashMap<Local, Place<'tcx>>,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        projected_reborrow_anchor_specs: &mut ReborrowAnchorSpecMap,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
        summary_elidable_shared_call_ref_locals: &HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
        trace_ssa_anchor: bool,
    ) {
        // Stack allocation lifetime: StorageLive/StorageDead.
        match stmt.kind {
            StatementKind::StorageDead(local) => {
                byte_copy_src_for_local.remove(&local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, local);
                let ty = body.local_decls[local].ty;
                if (self.is_shadowable_ptr_ty(tcx, body, ty)
                    || self.ty_contains_pointer_fields(tcx, body, ty, 8))
                    && !self.should_skip_storage_dead_shadow_kill(
                        tcx, body, block_data, stmt_idx, local,
                    )
                {
                    // Stack slots for pointer-carrying locals are often reused for unrelated
                    // scalars later in optimized MIR. Clear any residual ptr-shadow metadata at
                    // the lifetime boundary so a later non-pointer local cannot inherit stale
                    // reference/raw lineage from the old occupant.
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: Place::from(local),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(tcx, body, ty, stmt.source_info.span),
                        },
                    });
                }
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // NOTE: optimized MIR can place StorageDead before the last use
                    // through an outstanding reference. Emitting dead-by-default here
                    // causes false UAFs, so this remains opt-in.
                    if !self.use_storage_dead_enabled() {
                        return;
                    }
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
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
                let ty = body.local_decls[local].ty;
                if self.is_shadowable_ptr_ty(tcx, body, ty)
                    || self.ty_contains_pointer_fields(tcx, body, ty, 8)
                {
                    // This cleanup is independent from stack-allocation tracking: optimized MIR
                    // regularly reuses caller-frame stack slots for short-lived callee temps
                    // such as `NonNull<T>` carriers. Clear any residual ptr-shadow as soon as
                    // the new local becomes live so projected field copies do not inherit
                    // stale lineage from the previous occupant.
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: Place::from(local),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(tcx, body, ty, stmt.source_info.span),
                        },
                    });
                }
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // Record pointer-typed locals only if their address is taken (interesting locals).
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
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

        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                byte_copy_src_for_local.remove(&dst_local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, dst_local);
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_pointer_ty(dst_ty)
                    && self.rhs_carries_boundary_recovered_ptr(
                        body,
                        rvalue,
                        boundary_recovered_ptr_locals,
                    )
                {
                    boundary_recovered_ptr_locals.insert(dst_local);
                }
                if self.is_one_byte_sized_ty(tcx, dst_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_one_byte_sized_ty(tcx, src_ty)
                            && self.place_contains_deref(src_place)
                        {
                            byte_copy_src_for_local.insert(dst_local, src_place);
                        }
                    }
                }
            }
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
                        if !skip_static_ref_read && !skip_vtable_read && self.is_pointer_ty(ptr_ty)
                        {
                            // Best-effort size: use the type of the dereferenced/read place.
                            let loaded_ty = p.ty(&body.local_decls, tcx).ty;
                            let read_ty = loaded_ty;
                            // Loading function pointers or vtable-like structs should not trigger
                            // memory access checks; treat these as benign metadata reads.
                            let skip_fn_ptr_read =
                                matches!(loaded_ty.kind(), TyKind::FnPtr(..) | TyKind::FnDef(..))
                                    || self.is_fn_table_adt_ty(tcx, loaded_ty);
                            let skip_vtable_field_read = match read_ty.kind() {
                                TyKind::FnPtr(..) | TyKind::FnDef(..) => true,
                                TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                                    self.is_fn_table_adt_ty(tcx, *pointee)
                                }
                                _ => self.is_fn_table_adt_ty(tcx, read_ty),
                            };
                            // Keep function-pointer and vtable-like metadata loads silent, but
                            // still instrument pointer-valued memory reads. We need those for
                            // unaligned ptr-to-ptr loads and similar cases where the loaded value
                            // is itself a pointer and the UB happens at the read.
                            if skip_fn_ptr_read || skip_vtable_field_read {
                                // Skip only the READ instrumentation; continue scanning this stmt.
                            } else {
                                let size_op = self.size_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
                                    loaded_ty,
                                    stmt.source_info.span,
                                );
                                let align_op = self.align_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
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
                                    kind: InstrKind::PtrRead {
                                        ptr_local,
                                        size_op,
                                        align_op,
                                    },
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
                    let align_op =
                        self.align_operand_for_deref(tcx, body, ptr_local, stmt.source_info.span);

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
                        kind: InstrKind::PtrWrite {
                            ptr_local,
                            size_op,
                            align_op,
                        },
                    });
                }
            }
        }

        // Direct stack-slot write: assignment to a tracked local/field without an initial Deref.
        // Use the local's reborrow anchor as the access tag so writes through the root local
        // invalidate stale children once that local has been borrowed. Keep this allow-untagged:
        // most locals are never borrowed, and a zero anchor should stay silent.
        if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
            let begins_with_deref = lhs_place
                .projection
                .iter()
                .next()
                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
            if !begins_with_deref {
                let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                if interesting_stack_locals.contains(&lhs_place.local)
                    && !self.is_pointer_ty(lhs_ty)
                {
                    let size_op =
                        self.size_operand_for_ty(tcx, body, lhs_ty, stmt.source_info.span);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::StackSlotWriteAllowUntagged {
                            local: lhs_place.local,
                            size_op,
                            align_op: self.align_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
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
                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                        if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_base_ty = body.local_decls[src_place.local].ty;
                            let shadow_load_ok = self.is_shadowable_ptr_ty(tcx, body, src_ty)
                                && (!matches!(src_ty.kind(), TyKind::Ref(..))
                                    || self.place_contains_deref(src_place)
                                    || !self.is_pointer_ty(src_base_ty));
                            // Projected pointer fields inside non-pointer carriers (for example
                            // `BytesMut.3` or `Option<&T>.0`) should load their leaf shadow
                            // when available. Falling back straight to the carrier-family
                            // reborrow path turns internal raw fields into stack-slot parents.
                            //
                            // Keep the old suppression only for by-value carrier reference
                            // fields: those still rely on the carrier-anchor repair path when
                            // an ABI copy did not reconstruct a concrete leaf shadow slot.
                            let projected_carrier_field_load = !src_place.projection.is_empty()
                                && !self.is_pointer_ty(src_base_ty)
                                && src_place
                                    .projection
                                    .iter()
                                    .all(|pe| !matches!(pe, ProjectionElem::Downcast(..)))
                                && matches!(src_ty.kind(), TyKind::Ref(..));
                            if !projected_carrier_field_load
                                && !src_place.projection.is_empty()
                                && shadow_load_ok
                            {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowLoad dst={:?} src={:?}",
                                    dst_local,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: src_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: self.place_contains_deref(src_place),
                                    },
                                });
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                                return;
                            }
                        }
                    }

                    let mut skip_tag_prop = false;

                    // Preserve lineage for direct projected pointer/reference copies like
                    // `_dst = copy (_agg.1: &mut T)`. Recovering only a base local here is often
                    // too coarse and can leave the destination untagged until first use.
                    let direct_projected_src_place = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op),
                        Rvalue::CopyForDeref(p) => Some(*p),
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
                    if let Some(src_place) = direct_projected_src_place {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if !src_place.projection.is_empty()
                            && self.is_pointer_ty(src_ty)
                            && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                        {
                            if self.log_enabled(PassLogLevel::Trace) {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][projected-src] dst={:?} src={:?} dst_ty={:?}",
                                    dst_local,
                                    src_place,
                                    dst_ty
                                );
                            }
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                                    let bk = match dst_ty.kind() {
                                        TyKind::Ref(_, _, Mutability::Mut) => BorrowKind::Mut {
                                            kind: MutBorrowKind::Default,
                                        },
                                        _ => BorrowKind::Shared,
                                    };
                                    InstrKind::Ref {
                                        bk,
                                        src: src_place,
                                        projected_reborrow_anchor_key: None,
                                    }
                                } else {
                                    InstrKind::Raw {
                                        is_mut,
                                        src: src_place,
                                    }
                                },
                            });
                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                            }
                            skip_tag_prop = true;
                        }
                    }

                    // Casts to raw pointers should create a fresh raw tag with parent lineage,
                    // rather than copying the source tag directly.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
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
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
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
                        let explicit_nonnull_transmute =
                            matches!(rvalue, Rvalue::Cast(CastKind::Transmute, ..))
                                && matches!(
                                    src_ty.kind(),
                                    TyKind::Adt(adt, _)
                                        if {
                                            let name = tcx.def_path_str(adt.did());
                                            name.contains("::NonNull") || name.contains("::Unique")
                                        }
                                );
                        if !self.is_pointer_ty(src_ty)
                            && !explicit_nonnull_transmute
                            && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                        {
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            let anchor_key = self.normalized_ptr_expr_key_for_rvalue(
                                body,
                                rvalue,
                                &block_data.statements,
                                stmt_idx,
                            );
                            let reused_anchor_state =
                                anchor_key.as_ref().and_then(|(key, _deps)| {
                                    self.reusable_ssa_anchor_for_expr(
                                        body,
                                        ssa_anchor_for_expr,
                                        key,
                                        dst_local,
                                        dst_ty,
                                    )
                                });
                            if let Some(anchor_state) = reused_anchor_state {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] reuse root-cast dst={:?} anchor={:?} source={:?} key={}",
                                        dst_local,
                                        anchor_state.local,
                                        anchor_state.source,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: match anchor_state.source {
                                        SsaAnchorSource::Tag => InstrKind::TagProp {
                                            dst: dst_local,
                                            src: anchor_state.local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                        SsaAnchorSource::RefAncestor => {
                                            InstrKind::TagPropFromRefAncestor {
                                                dst: dst_local,
                                                src: anchor_state.local,
                                            }
                                        }
                                    },
                                });
                            } else {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] store root-cast dst={:?} key={}",
                                        dst_local,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: true,
                                    },
                                });
                                if let Some((key, deps)) = anchor_key {
                                    ssa_anchor_for_expr.insert(
                                        key,
                                        SsaAnchorState {
                                            local: dst_local,
                                            deps,
                                            source: SsaAnchorSource::Tag,
                                        },
                                    );
                                }
                            }
                            skip_tag_prop = true;
                        }
                    }
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op).and_then(|p| p.as_local()),
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
                        Rvalue::BinaryOp(BinOp::Offset, ops) => {
                            self.place_from_operand(&ops.0).and_then(|p| p.as_local())
                        }
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute,
                            op,
                            _to_ty,
                        ) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        _ => None,
                    };

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

                    let mut src_local_opt = src_local_opt;
                    if src_local_opt.is_none() {
                        if let Some(p) = projected_ptr_place {
                            src_local_opt = self.recover_pointer_source_local_for_projected_place(
                                tcx, body, bb, stmt_idx, p, false,
                            );
                        }
                    }

                    if !skip_tag_prop {
                        let mut handled_ptr_tag = false;
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
                                        if matches!(
                                            *op,
                                            BinOp::Offset | BinOp::Add | BinOp::Sub
                                        ) =>
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
                                        Operand::Copy(p) | Operand::Move(p) => {
                                            !p.projection.is_empty()
                                        }
                                        _ => false,
                                    },
                                    _ => false,
                                };

                                if rhs_requires_retag {
                                    let anchor_key = projected_ptr_place.and_then(|src_place| {
                                        self.projected_reborrow_anchor_allowed_for_src_place(
                                            body, src_place,
                                        )
                                        .then(|| {
                                            self.normalized_ptr_copy_anchor_key(
                                                tcx,
                                                body,
                                                rvalue,
                                                &block_data.statements,
                                                stmt_idx,
                                            )
                                        })
                                        .flatten()
                                    });
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });

                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse ptr-derive dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store ptr-derive dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
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
                                                strict_validity: false,
                                            },
                                        });
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
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
                                    self.rebind_ssa_anchors_for_copy(
                                        ssa_anchor_for_expr,
                                        src_local,
                                        dst_local,
                                    );
                                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::PtrUse {
                                                ptr_local: dst_local,
                                            },
                                        });
                                    }
                                }
                                tagged_ptr_locals.insert(dst_local);
                                if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::ShadowStore {
                                            src_local: dst_local,
                                        },
                                    });
                                }
                                handled_ptr_tag = true;
                            }
                        }
                        if !handled_ptr_tag {
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

                            if matches!(rvalue, Rvalue::CopyForDeref(_))
                                && self.log_enabled(PassLogLevel::Trace)
                            {
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
                                // (including call operands) do not see UNKNOWN_TAG. Preserve
                                // reference semantics for projected `&T` / `&mut T` copies;
                                // rooting them as raw pointers loses ref-kind behavior and can
                                // desynchronize later access metadata from the actual pointee.
                                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                    let is_mut = self.ptr_is_mut(dst_ty);
                                    let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                                    let projected_src_place = match rvalue {
                                        Rvalue::Use(op) => self.place_from_operand(op),
                                        Rvalue::CopyForDeref(p) => Some(*p),
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
                                    let anchor_key = self.normalized_ptr_copy_anchor_key(
                                        tcx,
                                        body,
                                        rvalue,
                                        &block_data.statements,
                                        stmt_idx,
                                    );
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });
                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse projected-root dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        let projected_reborrow_anchor_key = projected_src_place
                                            .and_then(|src_place| {
                                                self.maybe_projected_reborrow_anchor_key(
                                                    body,
                                                    src_place,
                                                    anchor_key.as_ref(),
                                                    projected_reborrow_anchor_specs,
                                                )
                                            });
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store projected-root dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: if let Some(src_place) = projected_src_place {
                                                if is_ref {
                                                    let bk = match dst_ty.kind() {
                                                        TyKind::Ref(_, _, Mutability::Mut) => {
                                                            BorrowKind::Mut {
                                                                kind: MutBorrowKind::Default,
                                                            }
                                                        }
                                                        _ => BorrowKind::Shared,
                                                    };
                                                    InstrKind::Ref {
                                                        bk,
                                                        src: src_place,
                                                        projected_reborrow_anchor_key,
                                                    }
                                                } else {
                                                    InstrKind::Raw {
                                                        is_mut,
                                                        src: src_place,
                                                    }
                                                }
                                            } else if is_ref {
                                                InstrKind::RetRoot {
                                                    dst_local,
                                                    is_mut,
                                                    is_ref: true,
                                                }
                                            } else {
                                                InstrKind::RawRoot {
                                                    ptr_local: dst_local,
                                                    is_mut,
                                                    exposed_provenance: matches!(
                                                        rvalue,
                                                        Rvalue::Cast(
                                                            CastKind::PointerWithExposedProvenance,
                                                            ..,
                                                        )
                                                    ),
                                                }
                                            },
                                        });
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
                                    tagged_ptr_locals.insert(dst_local);
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
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
                                                    exposed_provenance: false,
                                                },
                                            });
                                            tagged_ptr_locals.insert(dst_local);
                                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                                insert_points.push(InsertPoint {
                                                    bb,
                                                    stmt_idx,
                                                    insert_before: false,
                                                    source_info: stmt.source_info,
                                                    place: Place::from(dst_local),
                                                    kind: InstrKind::ShadowStore {
                                                        src_local: dst_local,
                                                    },
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Pointer-shadow maintenance for memory-resident pointer slots.
        if let StatementKind::Assign(box (lhs_place, rvalue)) = &stmt.kind {
            let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;

            if !lhs_place.projection.is_empty() {
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place)) = rvalue
                {
                    if src_place.ty(&body.local_decls, tcx).ty == lhs_ty {
                        let lhs_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, lhs_ty, 3);
                        let dst_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *lhs_place, lhs_ty);
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, lhs_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowStore dst_field={:?} src_local={:?}",
                                        dst_field,
                                        src_field.local
                                    );
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowCopySlot dst_field={:?} src={:?}",
                                        dst_field,
                                        src_field
                                    );
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                            return;
                        }
                        if lhs_has_ptr_fields && lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Projected Aggregate ShadowCopyRange dst={:?} src={:?}",
                                lhs_place,
                                src_place
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                            return;
                        }
                    }
                }

                if self.place_contains_deref(lhs_place.clone())
                    && self.is_one_byte_sized_ty(tcx, lhs_ty)
                {
                    let byte_copy_src = match rvalue {
                        Rvalue::Use(Operand::Copy(src_place))
                        | Rvalue::Use(Operand::Move(src_place)) => src_place
                            .as_local()
                            .and_then(|src_local| byte_copy_src_for_local.get(&src_local).copied()),
                        _ => None,
                    };
                    if let Some(src_place) = byte_copy_src {
                        rz_pass_trace!(
                            self,
                            "[rusteze][ptr-shadow] ShadowCopyRange dst={:?} src={:?} size=1",
                            lhs_place,
                            src_place
                        );
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowCopyRange {
                                src_place,
                                size_op: SizeOperand::Const(self.const_usize(
                                    tcx,
                                    stmt.source_info.span,
                                    1,
                                )),
                            },
                        });
                        return;
                    }
                }

                if self.is_shadowable_ptr_ty(tcx, body, lhs_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_shadowable_ptr_ty(tcx, body, src_ty) {
                            if src_place.projection.is_empty() {
                                ptr_locals_needing_tag.insert(src_place.local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowStore dst={:?} src_local={:?}",
                                    lhs_place,
                                    src_place.local
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowStore {
                                        src_local: src_place.local,
                                    },
                                });
                            } else {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowCopySlot dst={:?} src={:?}",
                                    lhs_place,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowCopySlot { src_place },
                                });
                            }
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    } else {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowKill {
                                size_op: self.size_operand_for_ty(
                                    tcx,
                                    body,
                                    lhs_ty,
                                    stmt.source_info.span,
                                ),
                            },
                        });
                    }
                } else if lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
                    });
                }
            } else if let Some(dst_local) = lhs_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place))
                | Rvalue::CopyForDeref(src_place) = rvalue
                {
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx,
                                body,
                                src_place.local,
                                dst_ty,
                            )
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                        }
                    }
                }
                match rvalue {
                    Rvalue::Aggregate(kind, ops) => {
                        let ops_vec: Vec<Operand<'tcx>> = ops.iter().cloned().collect();
                        self.emit_aggregate_shadow_ops(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            stmt.source_info,
                            dst_local,
                            dst_ty,
                            kind,
                            &ops_vec,
                            insert_points,
                            ptr_locals_needing_tag,
                        );
                    }
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place))
                    | Rvalue::CopyForDeref(src_place)
                        if src_place.ty(&body.local_decls, tcx).ty == dst_ty =>
                    {
                        let dst_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, dst_ty, 3);
                        let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(dst_local),
                            dst_ty,
                        );
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, dst_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                        } else if dst_has_ptr_fields && dst_ty.is_sized(tcx, body.typing_env(tcx)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Wrapper-carrier transmute: optimized MIR frequently wraps a pointer local into a
        // `NonNull<T>`/`Unique<T>`-style carrier via `Transmute`. Restore the destination leaf
        // shadow so later projected loads from the wrapper field do not degrade to UNKNOWN_TAG.
        if let StatementKind::Assign(box (dst_place, Rvalue::Cast(CastKind::Transmute, op, _))) =
            &stmt.kind
        {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    );
                    if !dst_leafs.is_empty() {
                        if let Some(src_place) = self.place_from_operand(op) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_leafs = self
                                .shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, src_ty);
                            if let Some(matched_leafs) =
                                self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                            {
                                for (dst_spec, src_spec) in matched_leafs {
                                    let dst_leaf = dst_spec.place;
                                    let src_leaf = src_spec.place;
                                    let kind = if src_leaf.projection.is_empty()
                                        && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                    {
                                        ptr_locals_needing_tag.insert(src_leaf.local);
                                        InstrKind::ShadowStore {
                                            src_local: src_leaf.local,
                                        }
                                    } else {
                                        InstrKind::ShadowCopySlot {
                                            src_place: src_leaf,
                                        }
                                    };
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: dst_leaf,
                                        kind,
                                    });
                                }
                            } else if self.ty_contains_pointer_fields(tcx, body, src_ty, 3)
                                && dst_ty.is_sized(tcx, body.typing_env(tcx))
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowKill {
                                        size_op: self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            dst_ty,
                                            stmt.source_info.span,
                                        ),
                                    },
                                });
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
                                name.contains("::NonNull") || name.contains("::Unique")
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
                            let src_place_opt = self.place_from_operand(op);
                            let direct_field_shadow = src_place_opt
                                .and_then(|src_place| src_place.as_local())
                                .and_then(|src_local| {
                                    self.first_direct_pointer_field_place(tcx, body, src_local)
                                });
                            if let Some(field_place) = direct_field_shadow {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: field_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: false,
                                    },
                                });
                                return;
                            }
                            let src_local_opt = src_place_opt
                                .and_then(|src_place| {
                                    if matches!(
                                        rvalue,
                                        Rvalue::Cast(CastKind::PointerWithExposedProvenance, ..,)
                                    ) {
                                        self.backtrack_global_exposed_provenance_source_local(
                                            body, dst_local,
                                        )
                                        .or_else(|| {
                                            src_place.as_local().and_then(|src_local| {
                                                if self
                                                    .is_pointer_ty(body.local_decls[src_local].ty)
                                                {
                                                    Some(src_local)
                                                } else {
                                                    self.backtrack_pointer_source_local(
                                                        body,
                                                        src_local,
                                                        &block_data.statements[..stmt_idx],
                                                    )
                                                    .or_else(|| {
                                                        self.backtrack_global_pointer_value_local(
                                                            body, src_local,
                                                        )
                                                    })
                                                }
                                            })
                                        })
                                    } else if let Some(src_local) = src_place.as_local() {
                                        if self.is_pointer_ty(body.local_decls[src_local].ty) {
                                            Some(src_local)
                                        } else {
                                            self.backtrack_pointer_source_local(
                                                body,
                                                src_local,
                                                &block_data.statements[..stmt_idx],
                                            )
                                            .or_else(
                                                || {
                                                    self.backtrack_global_pointer_value_local(
                                                        body, src_local,
                                                    )
                                                },
                                            )
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .and_then(|src_local| {
                                    let Some(src_place) = src_place_opt else {
                                        return Some(src_local);
                                    };
                                    let src_place_ty = src_place.ty(&body.local_decls, tcx).ty;
                                    let projected_carrier_load = !self.is_pointer_ty(src_place_ty)
                                        && !src_place.projection.is_empty()
                                        && src_local == src_place.local;
                                    if projected_carrier_load {
                                        None
                                    } else {
                                        Some(src_local)
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
                                        strict_validity: false,
                                    },
                                });
                            } else if let Some(src_place) = src_place_opt {
                                if !is_ref {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place,
                                        },
                                    });
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::RawRoot {
                                            ptr_local: dst_local,
                                            is_mut,
                                            exposed_provenance: false,
                                        },
                                    });
                                }
                            } else {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: false,
                                    },
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
                                        TyKind::Ref(_, _ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
                                        TyKind::RawPtr(_ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
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
                                            strict_validity: true,
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
                let black_box_sink_ref_temp = if matches!(bk, BorrowKind::Shared)
                    && stmt_idx + 1 == block_data.statements.len()
                {
                    if let Some(Terminator {
                        kind:
                            TerminatorKind::Call {
                                func,
                                args,
                                destination,
                                ..
                            },
                        ..
                    }) = &block_data.terminator
                    {
                        let call_dest_loc = Location {
                            block: bb,
                            statement_index: block_data.statements.len(),
                        };
                        let call_returns_dead = destination.as_local().is_some_and(|local| {
                            !self.local_has_observable_use_excluding_call_dest(
                                body,
                                local,
                                call_dest_loc,
                            )
                        });
                        let call_is_black_box = self
                            .direct_callee(tcx, body, block_data, func)
                            .map(|(did, _)| tcx.def_path_str(did).contains("black_box"))
                            .unwrap_or(false);
                        let arg_matches = args.iter().any(|arg| {
                            self.place_from_operand(&arg.node)
                                .is_some_and(|arg_place| arg_place.as_local() == Some(lhs_local))
                        });
                        call_returns_dead && call_is_black_box && arg_matches
                    } else {
                        false
                    }
                } else {
                    false
                };
                if self.is_pointer_ty(lhs_ty)
                    && !summary_elidable_shared_call_ref_locals.contains(&lhs_local)
                    && !black_box_sink_ref_temp
                {
                    let allow_projected_anchor =
                        self.projected_reborrow_anchor_allowed_for_src_place(body, *src_place);
                    let anchor_key = allow_projected_anchor
                        .then(|| {
                            self.normalized_ptr_expr_key_for_ref_source_place(
                                body,
                                *src_place,
                                &block_data.statements,
                                stmt_idx,
                            )
                        })
                        .flatten();
                    let reused_anchor_state = if allow_projected_anchor
                        && self.allow_ssa_anchor_reuse_for_ref_source_place(body, *bk, *src_place)
                    {
                        anchor_key.as_ref().and_then(|(key, _deps)| {
                            self.reusable_ssa_anchor_for_ref_source_expr(
                                body,
                                ssa_anchor_for_expr,
                                key,
                                lhs_local,
                            )
                        })
                    } else {
                        None
                    };
                    let ref_src_place = reused_anchor_state
                        .as_ref()
                        .map(|anchor_state| Place::from(anchor_state.local))
                        .unwrap_or(*src_place);
                    let projected_reborrow_anchor_key = self.maybe_projected_reborrow_anchor_key(
                        body,
                        *src_place,
                        anchor_key.as_ref(),
                        projected_reborrow_anchor_specs,
                    );

                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Ref {
                            bk: *bk,
                            src: ref_src_place,
                            projected_reborrow_anchor_key,
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    if let Some(anchor_state) = reused_anchor_state.as_ref() {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] reuse ref parent dst={:?} anchor={:?} source={:?} key={}",
                                lhs_local,
                                anchor_state.local,
                                anchor_state.source,
                                anchor_key
                                    .as_ref()
                                    .map(|(key, _)| key.as_str())
                                    .unwrap_or("<none>")
                            );
                        }
                    }
                    if let Some((key, deps)) = anchor_key {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] store ref-ancestor dst={:?} key={}",
                                lhs_local,
                                key
                            );
                        }
                        ssa_anchor_for_expr.insert(
                            key,
                            SsaAnchorState {
                                local: lhs_local,
                                deps,
                                source: SsaAnchorSource::RefAncestor,
                            },
                        );
                    }
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
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
                        kind: InstrKind::Raw {
                            is_mut,
                            src: src_place.clone(),
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
                }
            }
        }
    }

    /// Centralized call-effect classifier ("table").
    ///
    /// This MUST be kept consistent with instrumentation emission so that
    /// `warn_unknown_call_if_needed` does not drift from actual handling.
    fn classify_call_effect(&self, def_path: &str) -> CallEffect {
        if def_path.contains("::black_box") {
            return CallEffect::PtrDerive;
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

    fn classify_instrumented_call_effect_from_summary<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        summary: &unsafe_dataflow::UnsafeFunctionSummary,
    ) -> Option<CallEffect> {
        let dst_ty = destination.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(dst_ty) {
            return None;
        }

        if summary.ptr_args().iter().any(|entry| {
            entry.reaches_direct_sink() || entry.escapes_to_unknown_boundary()
        }) {
            return None;
        }

        let mut forwarded = summary.ptr_args().iter().filter(|entry| entry.forwarded_to_return());
        let forwarded_arg = forwarded.next()?;
        if forwarded.next().is_some() {
            return None;
        }

        let Some(arg) = args.get(forwarded_arg.arg_index()) else {
            return None;
        };
        let Some(arg_place) = self.place_from_operand(&arg.node) else {
            return None;
        };
        let arg_ty = arg_place.ty(&body.local_decls, tcx).ty;
        if !self.is_pointer_ty(arg_ty) {
            return None;
        }

        Some(CallEffect::PtrDerive)
    }

    fn pointer_place_from_rvalue<'tcx>(&self, rvalue: &Rvalue<'tcx>) -> Option<Place<'tcx>> {
        match rvalue {
            Rvalue::Use(Operand::Copy(place)) | Rvalue::Use(Operand::Move(place)) => Some(*place),
            Rvalue::CopyForDeref(place) => Some(*place),
            _ => None,
        }
    }

    fn operand_mentions_local<'tcx>(&self, operand: &Operand<'tcx>, local: Local) -> bool {
        self.place_from_operand(operand)
            .and_then(|place| place.as_local())
            == Some(local)
    }

    fn should_skip_storage_dead_shadow_kill<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        stmt_idx: usize,
        local: Local,
    ) -> bool {
        let Some(prev_stmt) = stmt_idx
            .checked_sub(1)
            .and_then(|idx| block_data.statements.get(idx))
        else {
            return false;
        };
        let StatementKind::Assign(box (dst_place, rvalue)) = &prev_stmt.kind else {
            return false;
        };
        if dst_place.as_local() == Some(local) {
            return false;
        }
        let dst_ty = dst_place.ty(&body.local_decls, tcx).ty;
        if !self.is_shadowable_ptr_ty(tcx, body, dst_ty)
            && !self.ty_contains_pointer_fields(tcx, body, dst_ty, 8)
        {
            return false;
        }
        match rvalue {
            Rvalue::Aggregate(_, ops) => {
                ops.iter().any(|op| self.operand_mentions_local(op, local))
            }
            Rvalue::Use(op) | Rvalue::Cast(_, op, _) => self.operand_mentions_local(op, local),
            Rvalue::CopyForDeref(place) => place.as_local() == Some(local),
            _ => false,
        }
    }

    fn place_contains_deref<'tcx>(&self, place: Place<'tcx>) -> bool {
        place
            .projection
            .iter()
            .any(|proj| matches!(proj, ProjectionElem::Deref))
    }

    fn pointer_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_local: Local,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        Place::from(base_local).project_deeper(
            &[PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty)],
            tcx,
        )
    }

    fn pointer_field_place_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_place: Place<'tcx>,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        base_place.project_deeper(
            &[PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty)],
            tcx,
        )
    }

    fn pointer_field_place_from_place_in_variant<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_place: Place<'tcx>,
        variant: Option<VariantIdx>,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        let mut elems = Vec::with_capacity(1 + usize::from(variant.is_some()));
        if let Some(variant_idx) = variant {
            elems.push(PlaceElem::Downcast(None, variant_idx));
        }
        elems.push(PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty));
        base_place.project_deeper(&elems, tcx)
    }

    fn aggregate_field_tys<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        dst_ty: Ty<'tcx>,
    ) -> Option<Vec<Ty<'tcx>>> {
        match dst_ty.kind() {
            TyKind::Tuple(field_tys) => Some(field_tys.iter().collect()),
            TyKind::Adt(adt, args) if adt.is_struct() => Some(
                adt.non_enum_variant()
                    .fields
                    .iter()
                    .map(|field| field.ty(tcx, args))
                    .collect(),
            ),
            _ => None,
        }
    }

    fn aggregate_field_specs_for_kind<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        dst_ty: Ty<'tcx>,
        aggregate_kind: &AggregateKind<'tcx>,
    ) -> Option<(Option<VariantIdx>, Vec<(usize, Ty<'tcx>)>)> {
        match (dst_ty.kind(), aggregate_kind) {
            (TyKind::Tuple(field_tys), AggregateKind::Tuple) => Some((
                None,
                field_tys
                    .iter()
                    .enumerate()
                    .map(|(idx, field_ty)| (idx, field_ty))
                    .collect(),
            )),
            (TyKind::Adt(adt, args), AggregateKind::Adt(_, variant_idx, _, _, active_field)) => {
                let variant = &adt.variant(*variant_idx);
                if let Some(active_field) = *active_field {
                    let field_ty = variant.fields[active_field].ty(tcx, args);
                    Some((
                        Some(*variant_idx),
                        vec![(active_field.as_usize(), field_ty)],
                    ))
                } else {
                    Some((
                        Some(*variant_idx),
                        variant
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(idx, field)| (idx, field.ty(tcx, args)))
                            .collect(),
                    ))
                }
            }
            _ => None,
        }
    }

    fn emit_aggregate_shadow_ops<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        aggregate_kind: &AggregateKind<'tcx>,
        ops: &[Operand<'tcx>],
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
    ) {
        let Some((variant, field_specs)) =
            self.aggregate_field_specs_for_kind(tcx, dst_ty, aggregate_kind)
        else {
            return;
        };

        for (field_idx, field_ty) in field_specs.into_iter() {
            if !self.is_shadowable_ptr_ty(tcx, body, field_ty)
                && !self.ty_contains_pointer_fields(tcx, body, field_ty, 3)
            {
                continue;
            }
            let Some(op) = ops.get(field_idx) else {
                continue;
            };
            let field_place = self.pointer_field_place_from_place_in_variant(
                tcx,
                Place::from(dst_local),
                variant,
                field_idx,
                field_ty,
            );
            let dst_leafs =
                self.shadowable_leaf_ptr_specs_from_place(tcx, body, field_place, field_ty);
            if dst_leafs.is_empty() {
                continue;
            }
            if let Some(src_place) = self.place_from_operand(op) {
                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                let src_leafs =
                    self.shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, src_ty);
                if let Some(matched_leafs) =
                    self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                {
                    for (dst_spec, src_spec) in matched_leafs {
                        let dst_leaf = dst_spec.place;
                        let dst_leaf_ty = dst_spec.ty;
                        let src_leaf = src_spec.place;
                        if src_leaf.projection.is_empty()
                            && self.is_shadowable_ptr_ty(tcx, body, dst_leaf_ty)
                        {
                            ptr_locals_needing_tag.insert(src_leaf.local);
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Aggregate ShadowStore dst_field={:?} src_local={:?}",
                                dst_leaf,
                                src_leaf.local
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info,
                                place: dst_leaf,
                                kind: InstrKind::ShadowStore {
                                    src_local: src_leaf.local,
                                },
                            });
                        } else {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Aggregate ShadowCopySlot dst_field={:?} src={:?}",
                                dst_leaf,
                                src_leaf
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info,
                                place: dst_leaf,
                                kind: InstrKind::ShadowCopySlot {
                                    src_place: src_leaf,
                                },
                            });
                        }
                    }
                    continue;
                }
                if field_ty.is_sized(tcx, body.typing_env(tcx)) {
                    rz_pass_trace!(
                        self,
                        "[rusteze][ptr-shadow] Aggregate ShadowCopyRange dst_field={:?} src={:?}",
                        field_place,
                        src_place
                    );
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info,
                        place: field_place,
                        kind: InstrKind::ShadowCopyRange {
                            src_place,
                            size_op: self.size_operand_for_ty(
                                tcx,
                                body,
                                field_ty,
                                source_info.span,
                            ),
                        },
                    });
                    continue;
                }
            }

            for dst_leaf_spec in dst_leafs {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info,
                    place: dst_leaf_spec.place,
                    kind: InstrKind::ShadowKill {
                        size_op: self.size_operand_for_ty(
                            tcx,
                            body,
                            dst_leaf_spec.ty,
                            source_info.span,
                        ),
                    },
                });
            }
        }
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
            SizeOperand::AlignOf(ty) => {
                let align_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(align_local),
                        Rvalue::NullaryOp(NullOp::AlignOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(align_local)), vec![stmt])
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
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
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
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                (Operand::Copy(Place::from(meta_local)), vec![meta_stmt])
            }
            SizeOperand::PtrMetadataAdtSlice {
                ptr_local,
                adt_ty,
                field_idx,
                elem_ty,
            } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let elem_size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let tail_bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let head_off_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let total_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                let elem_size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(elem_size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let tail_bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(tail_bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(elem_size_local)),
                            )),
                        ),
                    ))),
                );
                let offset_of_list = tcx.mk_offset_of(&[(VariantIdx::from_u32(0), *field_idx)]);
                let head_off_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(head_off_local),
                        Rvalue::NullaryOp(NullOp::OffsetOf(offset_of_list), *adt_ty),
                    ))),
                );
                let total_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(total_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(head_off_local)),
                                Operand::Copy(Place::from(tail_bytes_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(total_local)),
                    vec![
                        meta_stmt,
                        elem_size_stmt,
                        tail_bytes_stmt,
                        head_off_stmt,
                        total_stmt,
                    ],
                )
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

    fn is_box_new_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.ends_with("::new")
    }

    fn is_box_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Adt(adt, _) => {
                let def_path = tcx.def_path_str(adt.did());
                def_path.contains("::boxed::Box") || def_path.contains("boxed::Box")
            }
            _ => false,
        }
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

    fn call_arg_lineage_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        arg_index: usize,
    ) -> Option<Local> {
        if let Some(src_local) =
            self.call_arg_pointer_source_local(tcx, body, block_data, args, arg_index)
        {
            return Some(src_local);
        }

        let first = args.get(arg_index)?;
        let arg_place = self.place_from_operand(&first.node)?;
        if arg_place.projection.is_empty() {
            Some(arg_place.local)
        } else {
            None
        }
    }

    fn backtrack_same_typed_call_result_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        agg_local: Local,
        wanted_ty: Ty<'tcx>,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for pred_bb in body.basic_blocks.indices() {
            let pred_data = &body.basic_blocks[pred_bb];
            let Some(term) = &pred_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args, destination, ..
            } = &term.kind
            else {
                continue;
            };

            if destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut candidates: HashSet<Local> = HashSet::new();
            for arg in args.iter() {
                let Some(arg_place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                if !arg_place.projection.is_empty() {
                    continue;
                }
                let arg_ty = arg_place.ty(&body.local_decls, tcx).ty;
                if arg_ty != wanted_ty || self.is_pointer_ty(arg_ty) {
                    continue;
                }
                if !self.supports_slot_family_local(tcx, body, arg_place.local) {
                    continue;
                }
                candidates.insert(arg_place.local);
                if candidates.len() > 1 {
                    return None;
                }
            }

            let Some(src_local) = candidates.into_iter().next() else {
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
                if let Some(src_local) =
                    self.call_arg_pointer_source_local(tcx, body, pred_data, args, arg_index)
                {
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

    /// Conservative global recovery for aggregate locals produced by a call result where the
    /// callee has exactly one recoverable pointer source argument across all definitions.
    ///
    /// This is the fallback needed for patterns like:
    /// - predecessor block: `_agg = iter.next()`
    /// - successor block: `_val = copy (((_agg as Some).0).1)`
    ///
    /// The extraction block is not necessarily the direct call target, so
    /// `backtrack_single_pointer_arg_call_result_source_local` can miss it.
    fn backtrack_global_pointer_arg_call_result_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        agg_local: Local,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for block_data in body.basic_blocks.iter() {
            let Some(term) = &block_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args, destination, ..
            } = &term.kind
            else {
                continue;
            };
            if destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut ptr_sources = HashSet::new();
            for arg_index in 0..args.len() {
                if let Some(src_local) =
                    self.call_arg_pointer_source_local(tcx, body, block_data, args, arg_index)
                {
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

    fn ptr_derive_call_requires_strict_validation(&self, def_path: &str) -> bool {
        def_path.contains("::ptr::")
            && (def_path.ends_with("::add")
                || def_path.ends_with("::sub")
                || def_path.ends_with("::offset")
                || def_path.ends_with("::wrapping_add")
                || def_path.ends_with("::wrapping_sub")
                || def_path.ends_with("::wrapping_offset")
                || def_path.ends_with("::byte_add")
                || def_path.ends_with("::byte_sub")
                || def_path.ends_with("::wrapping_byte_add")
                || def_path.ends_with("::wrapping_byte_sub"))
    }

    fn local_is_temp_like<'tcx>(&self, body: &Body<'tcx>, local: Local) -> bool {
        if local == RETURN_PLACE || body.args_iter().any(|arg| arg == local) {
            return false;
        }

        if body.var_debug_info.iter().any(|info| {
            let VarDebugInfoContents::Place(place) = info.value else {
                return false;
            };
            place.projection.is_empty() && place.local == local
        }) {
            return false;
        }

        match body.local_decls[local].local_info.as_ref() {
            rustc_middle::mir::ClearCrossCrate::Set(info) => !matches!(
                &**info,
                rustc_middle::mir::LocalInfo::User(_)
                    | rustc_middle::mir::LocalInfo::StaticRef { .. }
                    | rustc_middle::mir::LocalInfo::ConstRef { .. }
            ),
            rustc_middle::mir::ClearCrossCrate::Clear => true,
        }
    }

    fn local_has_explicit_storage<'tcx>(&self, body: &Body<'tcx>, local: Local) -> bool {
        body.basic_blocks.iter().any(|block_data| {
            block_data.statements.iter().any(|stmt| {
                matches!(
                    stmt.kind,
                    StatementKind::StorageLive(l) | StatementKind::StorageDead(l) if l == local
                )
            })
        })
    }

    fn push_ptr_derive_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        src_local: Local,
        strict_validity: bool,
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
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        } else {
            // Fallback (should not happen for normal calls): keep the old placement.
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        }
    }

    fn push_ptr_derive_parent_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        strict_validity: bool,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        let is_mut = match dst_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        };
        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        tagged_ptr_locals.insert(dst_local);
        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDeriveParent {
                    dst: dst_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        } else {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDeriveParent {
                    dst: dst_local,
                    is_mut,
                    is_ref,
                    strict_validity,
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
                    exposed_provenance: false,
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
                    exposed_provenance: false,
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
                let effect = call_effect_opt.unwrap_or_else(|| self.classify_call_effect(def_path));
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
                    if *count < limit && filter.as_ref().map_or(true, |f| def_path.contains(f)) {
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
                let src_local = src_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let dst_local = dst_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let count_op = &args[2].node;

                let size_op_for = |ptr_local: Local| -> SizeOperand<'tcx> {
                    self.memop_size_bytes(tcx, body, ptr_local, count_op, term.source_info.span)
                };

                // These hooks are inserted via terminator splitting. Because later insert points
                // execute earlier, push destination WRITE before source READ so execution is:
                //   1. READ src
                //   2. WRITE dst
                //   3. perform the actual memcopy call
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
                        kind: InstrKind::PtrWrite {
                            ptr_local: dst,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                }
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
                        kind: InstrKind::PtrRead {
                            ptr_local: src,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                }
                if let (Some(src_place), Some(dst_place)) = (src_place, dst_place) {
                    let size_op = if let Some(src) = src_local {
                        size_op_for(src)
                    } else if let Some(dst) = dst_local {
                        size_op_for(dst)
                    } else {
                        SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0))
                    };
                    let shadow_bb = match &term.kind {
                        TerminatorKind::Call {
                            target: Some(tgt_bb),
                            ..
                        } => *tgt_bb,
                        _ => bb,
                    };
                    let shadow_stmt_idx = if shadow_bb == bb {
                        block_data.statements.len()
                    } else {
                        0
                    };
                    insert_points.push(InsertPoint {
                        bb: shadow_bb,
                        stmt_idx: shadow_stmt_idx,
                        insert_before: shadow_bb != bb,
                        source_info: term.source_info,
                        place: dst_place,
                        kind: InstrKind::ShadowCopyRange { src_place, size_op },
                    });
                }
            }
        } else if is_memset {
            // Signature convention:
            //   write_bytes::<T>(dst: *mut T, val: u8, count: usize)
            if args.len() >= 3 {
                let dst_place = args.get(0).and_then(|a| self.place_from_operand(&a.node));
                let dst_local = dst_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let count_op = &args[2].node;
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size_op =
                        self.memop_size_bytes(tcx, body, dst, count_op, term.source_info.span);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: dst_place.unwrap_or(Place::from(dst)),
                        kind: InstrKind::PtrWrite {
                            ptr_local: dst,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                    if let Some(dst_place) = dst_place {
                        let shadow_bb = match &term.kind {
                            TerminatorKind::Call {
                                target: Some(tgt_bb),
                                ..
                            } => *tgt_bb,
                            _ => bb,
                        };
                        let shadow_stmt_idx = if shadow_bb == bb {
                            block_data.statements.len()
                        } else {
                            0
                        };
                        insert_points.push(InsertPoint {
                            bb: shadow_bb,
                            stmt_idx: shadow_stmt_idx,
                            insert_before: shadow_bb != bb,
                            source_info: term.source_info,
                            place: dst_place,
                            kind: InstrKind::ShadowKill {
                                size_op: self.memop_size_bytes(
                                    tcx,
                                    body,
                                    dst,
                                    count_op,
                                    term.source_info.span,
                                ),
                            },
                        });
                    }
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
                                insert_before: true,
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
                                insert_before: true,
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
        boundary_recovered_ptr_locals: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        projectionless_anchor_suppressed_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        local_slot_shadow_store_locals: &mut HashSet<Local>,
        local_ref_use_stats: &HashMap<Local, LocalRefUseStats>,
    ) {
        let callee_opt = self.direct_callee(tcx, body, block_data, func);
        let callee_id_opt = callee_opt.map(|(_did, cid)| cid);
        let callee_path_opt = callee_opt.map(|(did, _)| tcx.def_path_str(did));
        let callee_summary_opt = callee_opt.and_then(|(did, _)| unsafe_dataflow::summary_for_def_id(tcx, did));
        let callee_instrumented = callee_opt
            .map(|(did, _)| self.is_instrumented_callee(tcx, did))
            .unwrap_or(false);
        let ret_take_enabled = self.ret_take_enabled();

        // 6a: Remove is_plain_store/is_plain_load computation.

        // Centralized effect classification for direct calls.
        let mut call_effect_opt: Option<CallEffect> = callee_path_opt
            .as_deref()
            .map(|p| self.classify_call_effect(p));
        if callee_instrumented && matches!(call_effect_opt, None | Some(CallEffect::Unknown)) {
            if let Some(summary) = callee_summary_opt.as_ref() {
                if let Some(summary_effect) = self.classify_instrumented_call_effect_from_summary(
                    tcx,
                    body,
                    args,
                    destination,
                    summary,
                ) {
                    call_effect_opt = Some(summary_effect);
                }
            }
        }
        let unknown_call =
            !callee_instrumented && matches!(call_effect_opt, None | Some(CallEffect::Unknown));

        // Unknown-call warnings are suppressed now that we instrument all non-std crates.

        let mut classified_write_ptr_local: Option<Local> = None;
        let mut classified_read_ptr_local: Option<Local> = None;
        let mut classified_derive_ptr_local: Option<Local> = None;
        let mut local_ptr_derive_emitted = false;
        let mut load_shadow_emitted = false;
        let unknown_call_returns_ptr =
            unknown_call && self.is_pointer_ty(destination.ty(&body.local_decls, tcx).ty);
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };
        let mut noescape_shared_reborrow_call_temps: HashSet<Local> = HashSet::new();
        for (arg_index, arg) in args.iter().enumerate() {
            if let Some(local) = self.noescape_shared_reborrow_call_temp_local(
                tcx,
                body,
                block_data,
                func,
                local_ref_use_stats,
                arg_index,
                arg,
            ) {
                noescape_shared_reborrow_call_temps.insert(local);
                local_slot_shadow_store_locals.insert(local);
            }
        }
        let call_is_black_box = callee_path_opt
            .as_deref()
            .is_some_and(|p| p.contains("black_box"));
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
                            AllocShimKind::Alloc
                                | AllocShimKind::AllocZeroed
                                | AllocShimKind::Realloc
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
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                                exposed_provenance: false,
                                            },
                                        });
                                    } else {
                                        // Fallback: if no target, place at end of current block.
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                                exposed_provenance: false,
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

                CallEffect::Store | CallEffect::StoreUnaligned => {
                    // store wrapper/intrinsic: WRITE through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                let prefer_source_tag = p0.local != ptr_local
                                    && self.alias_exempt_for_ptr_ty(
                                        tcx,
                                        body,
                                        body.local_decls[p0.local].ty,
                                    );
                                let hook_ptr_local = if p0.projection.is_empty()
                                    && self.is_pointer_ty(body.local_decls[p0.local].ty)
                                {
                                    if p0.local != ptr_local {
                                        ptr_locals_needing_tag.insert(p0.local);
                                        ptr_locals_needing_tag.insert(ptr_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: p0,
                                            kind: InstrKind::TagProp {
                                                dst: p0.local,
                                                src: ptr_local,
                                                copy_tag: true,
                                                copy_ref_ancestor: true,
                                            },
                                        });
                                    }
                                    if prefer_source_tag {
                                        ptr_local
                                    } else {
                                        p0.local
                                    }
                                } else {
                                    ptr_local
                                };
                                classified_write_ptr_local = Some(hook_ptr_local);
                                ptr_locals_needing_tag.insert(hook_ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let (size_op, align_op) = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl)
                                    | TyKind::Ref(_, pointee_ty, _mutbl) => (
                                        self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            *pointee_ty,
                                            term.source_info.span,
                                        ),
                                        if matches!(effect, CallEffect::StoreUnaligned) {
                                            SizeOperand::Const(self.const_usize(
                                                tcx,
                                                term.source_info.span,
                                                1,
                                            ))
                                        } else {
                                            self.align_operand_for_ty(
                                                tcx,
                                                body,
                                                *pointee_ty,
                                                term.source_info.span,
                                            )
                                        },
                                    ),
                                    _ => (
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                    ),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrWrite {
                                        ptr_local: hook_ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });
                            }
                        }
                    }
                }

                CallEffect::Load | CallEffect::LoadUnaligned => {
                    // load wrapper/intrinsic: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                let hook_ptr_local = if p0.projection.is_empty()
                                    && self.is_pointer_ty(body.local_decls[p0.local].ty)
                                {
                                    if p0.local != ptr_local {
                                        ptr_locals_needing_tag.insert(p0.local);
                                        ptr_locals_needing_tag.insert(ptr_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: p0,
                                            kind: InstrKind::TagProp {
                                                dst: p0.local,
                                                src: ptr_local,
                                                copy_tag: true,
                                                copy_ref_ancestor: true,
                                            },
                                        });
                                    }
                                    p0.local
                                } else {
                                    ptr_local
                                };
                                classified_read_ptr_local = Some(hook_ptr_local);
                                ptr_locals_needing_tag.insert(hook_ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let (size_op, align_op) = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl)
                                    | TyKind::Ref(_, pointee_ty, _mutbl) => (
                                        self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            *pointee_ty,
                                            term.source_info.span,
                                        ),
                                        if matches!(effect, CallEffect::LoadUnaligned) {
                                            SizeOperand::Const(self.const_usize(
                                                tcx,
                                                term.source_info.span,
                                                1,
                                            ))
                                        } else {
                                            self.align_operand_for_ty(
                                                tcx,
                                                body,
                                                *pointee_ty,
                                                term.source_info.span,
                                            )
                                        },
                                    ),
                                    _ => (
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                    ),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrRead {
                                        ptr_local: hook_ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });

                                if let (Some(dst_local), Some(tgt_bb)) =
                                    (destination.as_local(), call_target_bb)
                                {
                                    let dst_ty = body.local_decls[dst_local].ty;
                                    let loaded_ptr_ty = match ty0.kind() {
                                        TyKind::RawPtr(pointee_ty, _mutbl)
                                        | TyKind::Ref(_, pointee_ty, _mutbl) => Some(*pointee_ty),
                                        _ => None,
                                    };
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty)
                                        && loaded_ptr_ty.is_some_and(|ty| {
                                            self.is_shadowable_ptr_ty(tcx, body, ty)
                                        })
                                    {
                                        ptr_locals_needing_tag.insert(dst_local);
                                        tagged_ptr_locals.insert(dst_local);
                                        let load_src_place =
                                            p0.project_deeper(&[PlaceElem::Deref], tcx);
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: load_src_place,
                                            kind: InstrKind::ShadowLoad {
                                                dst_local,
                                                require_tag: true,
                                                validate_ref: false,
                                            },
                                        });
                                        load_shadow_emitted = true;
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::CarrierCopyArg0 => {
                    if let (Some(dst_local), Some(tgt_bb)) =
                        (destination.as_local(), call_target_bb)
                    {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if !self.is_pointer_ty(dst_ty) {
                            if let Some(src_place) = args
                                .get(0)
                                .and_then(|arg| self.place_from_operand(&arg.node))
                            {
                                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                    tcx,
                                    body,
                                    Place::from(dst_local),
                                    dst_ty,
                                );
                                let src_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                    tcx, body, src_place, src_ty,
                                );
                                if let Some(matched_leafs) =
                                    self.pair_shadowable_leaf_ptr_specs_from_arg0(
                                        &dst_leafs,
                                        &src_leafs,
                                    )
                                {
                                    for (dst_spec, src_spec) in matched_leafs {
                                        let kind = if src_spec.place.projection.is_empty()
                                            && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                        {
                                            ptr_locals_needing_tag.insert(src_spec.place.local);
                                            InstrKind::ShadowStore {
                                                src_local: src_spec.place.local,
                                            }
                                        } else {
                                            InstrKind::ShadowCopySlot {
                                                src_place: src_spec.place,
                                            }
                                        };
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: dst_spec.place,
                                            kind,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::PtrDerive => {
                    // Pointer-result handling for ptr-derivation wrappers (add/sub/offset/as_ptr...).
                    //
                    // For raw-pointer wrappers, local PtrDerive is more robust than relying on
                    // return-boundary transport alone: tiny helpers like `as_mut_ptr` often return
                    // a projected pointer value, and if the callee never materializes a precise
                    // return tag, later dereferences degrade into UNKNOWN_TAG.
                    //
                    // Shared-ref wrappers are different. If an instrumented callee returns `&T`,
                    // the return-boundary transport (`RetPush`/`RetTake`) is the principled model:
                    // it preserves the boundary parent that the next call should retag from. A
                    // local PtrDerive on the caller side collapses that boundary state back onto
                    // the receiver/source tag and can produce stale shared children at the next
                    // call boundary (for example `Bytes::as_ref()` -> subslice -> `slice_ref`).
                    if !call_is_black_box {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            let prefer_return_boundary_for_ref = callee_instrumented
                                && ret_take_enabled
                                && matches!(dst_ty.kind(), TyKind::Ref(..));
                            // Allow wide-pointer destinations too (e.g., from_raw_parts_mut -> &mut [T]).
                            if self.is_pointer_ty(dst_ty) && !prefer_return_boundary_for_ref {
                                let mut derive_emitted_here = false;
                                let mut derived_from_recovered_boundary = false;
                                let src_arg_index = callee_path_opt
                                    .as_deref()
                                    .map(|p| self.ptr_derive_source_arg_index(p))
                                    .unwrap_or(0);
                                let src_arg_place = args
                                    .get(src_arg_index)
                                    .and_then(|arg| self.place_from_operand(&arg.node));
                                let prefer_parent_snapshot =
                                    src_arg_place.is_some_and(|src_place| {
                                        let src_place_ty = src_place.ty(&body.local_decls, tcx).ty;
                                        !src_place.projection.is_empty()
                                            || !self.is_pointer_ty(src_place_ty)
                                    });
                                if prefer_parent_snapshot {
                                    if let Some(src_place) = src_arg_place {
                                        if boundary_recovered_ptr_locals.contains(&src_place.local)
                                        {
                                            derived_from_recovered_boundary = true;
                                        }
                                        ptr_locals_needing_tag.insert(dst_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: src_place,
                                            kind: InstrKind::ParentTagSnapshot {
                                                dst_local,
                                                src: src_place,
                                                is_raw_creation: !matches!(
                                                    dst_ty.kind(),
                                                    TyKind::Ref(..)
                                                ),
                                            },
                                        });
                                        Self::push_ptr_derive_parent_call(
                                            bb,
                                            block_data,
                                            term,
                                            dst_local,
                                            dst_ty,
                                            callee_path_opt.as_deref().is_some_and(|p| {
                                                self.ptr_derive_call_requires_strict_validation(p)
                                            }),
                                            insert_points,
                                            tagged_ptr_locals,
                                        );
                                        local_ptr_derive_emitted = true;
                                        derive_emitted_here = true;
                                    }
                                } else if let Some(src_local) = self.call_arg_pointer_source_local(
                                    tcx,
                                    body,
                                    block_data,
                                    args,
                                    src_arg_index,
                                ) {
                                    if boundary_recovered_ptr_locals.contains(&src_local) {
                                        derived_from_recovered_boundary = true;
                                    }
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);
                                    Self::push_ptr_derive_call(
                                        bb,
                                        block_data,
                                        term,
                                        dst_local,
                                        dst_ty,
                                        src_local,
                                        callee_path_opt.as_deref().is_some_and(|p| {
                                            self.ptr_derive_call_requires_strict_validation(p)
                                        }),
                                        insert_points,
                                        tagged_ptr_locals,
                                        &mut classified_derive_ptr_local,
                                    );
                                    local_ptr_derive_emitted = true;
                                    derive_emitted_here = true;
                                }
                                if derive_emitted_here && derived_from_recovered_boundary {
                                    boundary_recovered_ptr_locals.insert(dst_local);
                                }
                                if derive_emitted_here
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_ty)
                                {
                                    if let Some(tgt_bb) = call_target_bb {
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::ExposedProvenanceRoot => {
                    if let Some(dst_local) = destination.as_local() {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            if let Some(tgt_bb) = call_target_bb {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut: self.ptr_is_mut(dst_ty),
                                        exposed_provenance: true,
                                    },
                                });
                                if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                    insert_points.push(InsertPoint {
                                        bb: tgt_bb,
                                        stmt_idx: 0,
                                        insert_before: true,
                                        source_info: term.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::ShadowStore {
                                            src_local: dst_local,
                                        },
                                    });
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

        if let (Some(tgt_bb), Some(dst_local), Some(first_arg)) = (
            call_target_bb,
            destination.as_local(),
            args.get(0)
                .and_then(|arg| self.place_from_operand(&arg.node)),
        ) {
            let dst_ty = body.local_decls[dst_local].ty;
            let src_ty = first_arg.ty(&body.local_decls, tcx).ty;
            if args.len() == 1
                && callee_path_opt
                    .as_deref()
                    .is_some_and(|p| self.is_box_new_wrapper(p))
                && self.is_box_ty(tcx, dst_ty)
                && self.is_shadowable_ptr_ty(tcx, body, src_ty)
                && first_arg.projection.is_empty()
            {
                ptr_locals_needing_tag.insert(first_arg.local);
                insert_points.push(InsertPoint {
                    bb: tgt_bb,
                    stmt_idx: 0,
                    insert_before: true,
                    source_info: term.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::ShadowStoreBoxPointee {
                        box_local: dst_local,
                        src_local: first_arg.local,
                    },
                });
            }
        }

        // Treat any pointer argument as tag relevant.
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else {
                continue;
            };
            let ty = p.ty(&body.local_decls, tcx).ty;
            if callee_instrumented
                && !self.is_pointer_ty(ty)
                && !p.projection.is_empty()
                && self.is_pointer_ty(body.local_decls[p.local].ty)
            {
                if let Some(callee_id) = callee_id_opt {
                    let exact_inplace_source =
                        p.projection.len() == 1 && matches!(p.projection[0], ProjectionElem::Deref);
                    let mut flags = self.call_arg_push_flags_for_ptr_local(
                        body,
                        p.local,
                        exact_inplace_source,
                        boundary_recovered_ptr_locals,
                    );
                    if self.local_is_direct_mut_ref_of_nonpointer_local(body, p.local) {
                        flags &= !CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE;
                    }
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
                            flags,
                        },
                    });
                }
            }
            if !self.is_pointer_ty(ty) && self.ty_contains_direct_pointer_fields(tcx, body, ty) {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind: InstrKind::CallArgValidate { local: p.local },
                });
            }
            if !self.is_pointer_ty(ty) {
                continue;
            }
            let is_addr_exposable = self.is_addr_exposable_ptr_ty(tcx, body, ty);

            // If this arg is `&mut P` (P pointer-typed), track the pointee local for
            // post-call retagging in the caller to avoid stale tags after writeback.
            if let TyKind::Ref(_, pointee_ty, mutbl) = ty.kind() {
                if matches!(mutbl, Mutability::Mut) && self.is_pointer_ty(*pointee_ty) {
                    if let Some(pointee_local) =
                        self.backtrack_mut_ref_pointee_local(body, p.local, &block_data.statements)
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
                    .recover_projected_pointer_rhs_source_local(
                        tcx,
                        body,
                        bb,
                        block_data.statements.len(),
                        p.local,
                    )
                    .or_else(|| {
                        self.backtrack_pointer_source_local(body, p.local, &block_data.statements)
                    })
                    .filter(|src_local| {
                        *src_local != p.local && tagged_ptr_locals.contains(src_local)
                    });
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
                            strict_validity: false,
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
                        InstrKind::RawRoot {
                            ptr_local: p.local,
                            is_mut,
                            exposed_provenance: false,
                        }
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
            if callee_instrumented && self.tb_call_arg_protector_supported_for_ty(tcx, body, ty) {
                if let Some(callee_id) = callee_id_opt {
                    ptr_locals_needing_tag.insert(p.local);
                    let mut flags = self.call_arg_push_flags_for_ptr_local(
                        body,
                        p.local,
                        false,
                        boundary_recovered_ptr_locals,
                    );
                    if self.local_is_direct_mut_ref_of_nonpointer_local(body, p.local) {
                        flags &= !CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE;
                    }
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
                            flags,
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
                let align_op = match ty.kind() {
                    TyKind::RawPtr(pointee_ty, _) | TyKind::Ref(_, pointee_ty, _) => {
                        self.align_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
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
                        align_op: align_op.clone(),
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
                        kind: InstrKind::PtrWriteAllowUntagged {
                            ptr_local: p.local,
                            size_op,
                            align_op,
                        },
                    });
                }
            }

            // Always record a coarse escape event for pointer arguments at call boundaries,
            // even when specific effects (read/write/derive) are also modeled.
            if self.filter_stdlib_uses_enabled() && self.span_is_stdlib(tcx, term.source_info.span)
            {
                continue;
            }
            let projected_carrier_raw_ptr_use = self.is_raw_pointer_ty(ty)
                && self.raw_creation_allows_no_provenance_transport(tcx, body, p);
            let noescape_shared_reborrow_temp =
                noescape_shared_reborrow_call_temps.contains(&p.local);
            if !projected_carrier_raw_ptr_use && !noescape_shared_reborrow_temp {
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
        }

        if callee_instrumented {
            if let Some(callee_id) = callee_id_opt {
                for (arg_index, a) in args.iter().enumerate() {
                    let Some(p) = self.place_from_operand(&a.node) else {
                        continue;
                    };
                    let ty = p.ty(&body.local_decls, tcx).ty;
                    if self.is_pointer_ty(ty) {
                        continue;
                    }
                    if !self.supports_call_boundary_anchor_local(tcx, body, p.local) {
                        continue;
                    }
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
                            flags: if let Some(anchor_ptr_local_ty) =
                                Some(body.local_decls[p.local].ty)
                            {
                                if matches!(
                                    anchor_ptr_local_ty.kind(),
                                    TyKind::Ref(_, pointee_ty, Mutability::Mut)
                                        if !self.is_pointer_ty(*pointee_ty)
                                ) {
                                    CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE
                                } else {
                                    0
                                }
                            } else {
                                0
                            },
                        },
                    });
                    if let Some(tgt_bb) = call_target_bb {
                        if self.is_shadowable_ptr_ty(tcx, body, ty) {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(p.local),
                                kind: InstrKind::ShadowStore { src_local: p.local },
                            });
                        }
                    }
                }
            }
        }

        if let Some(tgt_bb) = call_target_bb {
            if let Some(dst_local) = destination.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) && interesting_stack_locals.contains(&dst_local) {
                    let mut recovered_src: Option<Local> = None;
                    let mut ambiguous = false;
                    for arg_index in 0..args.len() {
                        if let Some(src_local) = self
                            .call_arg_lineage_source_local(tcx, body, block_data, args, arg_index)
                        {
                            match recovered_src {
                                Some(existing) if existing != src_local => {
                                    ambiguous = true;
                                    break;
                                }
                                Some(_) => {}
                                None => recovered_src = Some(src_local),
                            }
                        }
                    }
                    if !ambiguous {
                        if let Some(src_local) = recovered_src {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if !self.ty_contains_direct_pointer_fields(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ReborrowAnchorSeed {
                                        dst_local,
                                        src_local,
                                        mark_slot_family: false,
                                    },
                                });
                            }
                        }
                    }
                }
            }
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
                ptr_locals_needing_tag.insert(dst_local);
                tagged_ptr_locals.insert(dst_local);
                if callee_instrumented && self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::ShadowLoad {
                            dst_local,
                            require_tag: true,
                            validate_ref: false,
                        },
                    });
                } else {
                    let is_mut = match dst_ty.kind() {
                        TyKind::Ref(_, _, mutbl) => matches!(mutbl, Mutability::Mut),
                        TyKind::RawPtr(_, mutbl) => matches!(mutbl, Mutability::Mut),
                        _ => false,
                    };
                    let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::RetRoot {
                            dst_local,
                            is_mut,
                            is_ref,
                        },
                    });
                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                        insert_points.push(InsertPoint {
                            bb: tgt_bb,
                            stmt_idx: 0,
                            insert_before: true,
                            source_info: term.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::ShadowStore {
                                src_local: dst_local,
                            },
                        });
                    }
                }
            }
        }

        if callee_instrumented {
            if let Some(callee_id) = callee_id_opt {
                for (arg_index, a) in args.iter().enumerate() {
                    let Some(p) = self.place_from_operand(&a.node) else {
                        continue;
                    };
                    let ty = p.ty(&body.local_decls, tcx).ty;
                    let TyKind::Ref(_, pointee_ty, mutbl) = ty.kind() else {
                        continue;
                    };
                    if !matches!(mutbl, Mutability::Mut) {
                        continue;
                    }
                    if self.is_pointer_ty(*pointee_ty) {
                        continue;
                    }
                    if let Some(pointee_local) =
                        self.backtrack_mut_ref_pointee_local(body, p.local, &block_data.statements)
                    {
                        if self.supports_call_boundary_anchor_local(tcx, body, pointee_local) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(pointee_local),
                                kind: InstrKind::MutArgRetTake {
                                    callee_id,
                                    arg_index: arg_index as u64,
                                    local: pointee_local,
                                    ptr_local: p.local,
                                },
                            });
                            let pointee_ty = body.local_decls[pointee_local].ty;
                            for dst_leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                                tcx,
                                body,
                                Place::from(pointee_local),
                                pointee_ty,
                            ) {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: dst_leaf_spec.place,
                                    kind: InstrKind::MutArgRetLeafTake {
                                        callee_id,
                                        arg_index: arg_index as u64,
                                        local: pointee_local,
                                        leaf_key: dst_leaf_spec.transport_key(),
                                    },
                                });
                            }
                            boundary_recovered_ptr_locals.insert(p.local);
                            boundary_recovered_ptr_locals.insert(pointee_local);
                            continue;
                        }
                    }

                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(p.local),
                        kind: InstrKind::MutArgRetTakePtrOnly {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                        },
                    });
                    boundary_recovered_ptr_locals.insert(p.local);
                }
            }
        }

        // Caller-side return recovery.
        //
        // Plain pointer returns use `RetRoot` / `RetTake`. Non-pointer carriers with embedded
        // pointer fields use `RetAnchorTake` / `RetAnchorRoot` so the destination local keeps a
        // whole-slot borrow family even though the MIR return place itself is not pointer-typed.
        //
        // Keep the carrier cases outside the pointer-return classifier: wrapper returns such as
        // `Result<(), BytesMut>` must still import or seed an anchor even though
        // `supports_call_boundary_ret_tag_ty` is false for the aggregate destination.
        //
        // For simple pointer-derivation wrappers like `as_mut_ptr` or `ptr.add`, we prefer the
        // local PtrDerive model over call-boundary return-tag recovery. This keeps provenance
        // stable even when the callee is instrumented but returns a projected pointer value.
        if let Some(dst_local) = destination.as_local().filter(|_| !call_is_black_box) {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.supports_call_boundary_ret_tag_ty(tcx, body, dst_ty) {
                if matches!(call_effect_opt, Some(CallEffect::PtrDerive))
                    && local_ptr_derive_emitted
                {
                    // Already modeled by the local PtrDerive insertion above.
                } else if callee_instrumented {
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
                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                            }
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
                            kind: InstrKind::RetTake {
                                callee_id,
                                dst_local,
                            },
                        });
                        boundary_recovered_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                            }
                        }
                    }
                } else {
                    // Uninstrumented callee: synthesize a fresh return tag unless another effect already
                    // models the return pointer (alloc shims/ptr-derive/Box::into_raw).
                    let alloc_returns_ptr = matches!(
                        call_effect_opt,
                        Some(CallEffect::AllocShim(
                            AllocShimKind::Alloc
                                | AllocShimKind::AllocZeroed
                                | AllocShimKind::Realloc
                        ))
                    );
                    let mut return_tagged_by_effect =
                        matches!(call_effect_opt, Some(CallEffect::BoxIntoRaw))
                            || (matches!(call_effect_opt, Some(CallEffect::PtrDerive))
                                && local_ptr_derive_emitted)
                            || matches!(call_effect_opt, Some(CallEffect::ExposedProvenanceRoot))
                            || (alloc_returns_ptr && !self.heap_allocs_from_mir_enabled())
                            || load_shadow_emitted;

                    // `core::intrinsics::read_via_copy` is classified as `Load`: when it returns
                    // a pointer value, that return is derived from arg0's pointer provenance.
                    if !return_tagged_by_effect && matches!(call_effect_opt, Some(CallEffect::Load))
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
                                false,
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
                                insert_before: true,
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
            } else if !self.is_pointer_ty(dst_ty)
                && callee_instrumented
                && self.supports_call_boundary_anchor_local(tcx, body, dst_local)
            {
                if let Some(callee_id) = callee_id_opt {
                    projectionless_anchor_suppressed_locals.insert(dst_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::RetAnchorTake {
                            callee_id,
                            local: dst_local,
                        },
                    });
                    for dst_leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: dst_leaf_spec.place,
                            kind: InstrKind::RetLeafTake {
                                callee_id,
                                leaf_key: dst_leaf_spec.transport_key(),
                            },
                        });
                    }
                }
            } else if !self.is_pointer_ty(dst_ty)
                && self.supports_call_boundary_anchor_local(tcx, body, dst_local)
            {
                projectionless_anchor_suppressed_locals.insert(dst_local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: true,
                    source_info: term.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::RetAnchorRoot { local: dst_local },
                });
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

        if let Some(target_bb) = call_target_bb {
            let dst_local = destination.as_local();
            let mut kill_locals: HashSet<Local> = HashSet::new();
            for arg in args.iter() {
                let Some(arg_place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                if dst_local == Some(arg_place.local) {
                    continue;
                }
                if noescape_shared_reborrow_call_temps.contains(&arg_place.local) {
                    kill_locals.insert(arg_place.local);
                }
            }
            let mut kill_locals: Vec<Local> = kill_locals.into_iter().collect();
            kill_locals.sort_by_key(|local| local.index());
            for ptr_local in kill_locals {
                insert_points.push(InsertPoint {
                    bb: target_bb,
                    stmt_idx: 0,
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(ptr_local),
                    kind: InstrKind::TagKill { ptr_local },
                });
            }
        }
    }

    fn scan_drop_terminator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        place: Place<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
    ) {
        let dropped_ty = place.ty(&body.local_decls, tcx).ty;
        let typing_env = body.typing_env(tcx);
        let dropped_ty = tcx
            .try_normalize_erasing_regions(typing_env, dropped_ty)
            .unwrap_or(dropped_ty);
        let drop_lang_item = tcx.require_lang_item(LangItem::DropInPlace, term.source_info.span);
        let drop_args = tcx.mk_args(&[dropped_ty.into()]);
        let mut callee_ids: Vec<u64> = Vec::new();
        if let Some(drop_def_id) = Instance::try_resolve(tcx, typing_env, drop_lang_item, drop_args)
            .ok()
            .flatten()
            .map(|instance| instance.def_id())
            .filter(|did| self.is_instrumented_callee(tcx, *did))
        {
            callee_ids.push(self.callee_id_u64(tcx, drop_def_id));
        }
        if let Some(dtor_def_id) = dropped_ty
            .ty_adt_def()
            .and_then(|adt| adt.destructor(tcx))
            .map(|dtor| dtor.did)
            .filter(|did| self.is_instrumented_callee(tcx, *did))
        {
            let callee_id = self.callee_id_u64(tcx, dtor_def_id);
            if !callee_ids.contains(&callee_id) {
                callee_ids.push(callee_id);
            }
        }
        if callee_ids.is_empty() {
            return;
        }

        if self.is_pointer_ty(dropped_ty) {
            if self.tb_call_arg_protector_supported_for_ty(tcx, body, dropped_ty) {
                ptr_locals_needing_tag.insert(place.local);
                tagged_ptr_locals.insert(place.local);
                for callee_id in callee_ids {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place,
                        kind: InstrKind::CallArgPush {
                            callee_id,
                            arg_index: 0,
                            ptr_local: place.local,
                            flags: self.call_arg_push_flags_for_ptr_local(
                                body,
                                place.local,
                                false,
                                boundary_recovered_ptr_locals,
                            ),
                        },
                    });
                }
            }
            return;
        }

        if self.ty_contains_direct_pointer_fields(tcx, body, dropped_ty) {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place,
                kind: InstrKind::CallArgValidate { local: place.local },
            });
        }

        if !self.supports_call_boundary_anchor_local(tcx, body, place.local) {
            return;
        }

        for callee_id in callee_ids {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place,
                kind: InstrKind::CallArgPush {
                    callee_id,
                    arg_index: 0,
                    ptr_local: place.local,
                    flags: 0,
                },
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
        let mut boundary_recovered_ptr_locals: HashSet<Local> = HashSet::new();
        let mut tagged_ptr_locals: HashSet<Local> = HashSet::new();
        let mut projectionless_anchor_suppressed_locals: HashSet<Local> = HashSet::new();
        let mut local_slot_shadow_store_locals: HashSet<Local> = HashSet::new();
        let local_ref_use_stats = self.compute_local_ref_use_stats(body);
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

        let mut interesting_stack_locals = self.compute_interesting_stack_locals(tcx, body);
        for arg_local in body.args_iter() {
            let arg_ty = body.local_decls[arg_local].ty;
            if !self.is_pointer_ty(arg_ty)
                && self.supports_slot_family_local(tcx, body, arg_local)
            {
                interesting_stack_locals.insert(arg_local);
            }
        }
        for block_data in body.basic_blocks.iter() {
            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            let TerminatorKind::Call { destination, .. } = &term.kind else {
                continue;
            };
            let Some(dst_local) = destination.as_local() else {
                continue;
            };
            let dst_ty = body.local_decls[dst_local].ty;
            if !self.is_pointer_ty(dst_ty)
                && self.supports_slot_family_local(tcx, body, dst_local)
            {
                interesting_stack_locals.insert(dst_local);
            }
        }
        for local in interesting_stack_locals.iter().copied() {
            if self.supports_call_boundary_anchor_local(tcx, body, local) {
                projectionless_anchor_suppressed_locals.insert(local);
            }
        }

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
            if !self.should_emit_stack_alloc_for_size_op(&size_op) {
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
            &mut local_slot_shadow_store_locals,
            &mut tagged_ptr_locals,
            &mut projectionless_anchor_suppressed_locals,
            &interesting_stack_locals,
            entry_insert_at,
        );
        for arg_local in body.args_iter() {
            if self.is_pointer_ty(body.local_decls[arg_local].ty) {
                boundary_recovered_ptr_locals.insert(arg_local);
            }
        }

        let mut return_sites: Vec<(BasicBlock, SourceInfo, usize)> = Vec::new();
        let predecessors = body.basic_blocks.predecessors();
        let ssa_anchor_entry_by_bb = self.analyze_ssa_anchor_entry_maps(
            tcx,
            body,
            &ptr_locals_with_tag_sources,
            &summary_elidable_shared_call_ref_locals,
            &interesting_stack_locals,
            track_all_stack_allocs,
        );
        let mut projected_reborrow_anchor_specs: ReborrowAnchorSpecMap = HashMap::new();

        for (bb, block_data) in traversal::preorder(body) {
            let mut byte_copy_src_for_local: HashMap<Local, Place<'tcx>> = HashMap::new();
            let mut ssa_anchor_for_expr: SsaAnchorMap =
                ssa_anchor_entry_by_bb.get(&bb).cloned().unwrap_or_default();
            let mut bb_projected_reborrow_anchor_specs: ReborrowAnchorSpecMap = HashMap::new();
            if self.log_enabled(PassLogLevel::Trace) {
                rz_pass_trace!(
                    self,
                    "[rusteze][ssa-anchor] enter bb={:?} pred_count={} anchor_count={}",
                    bb,
                    predecessors[bb].len(),
                    ssa_anchor_for_expr.len(),
                );
            }
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                self.scan_statement(
                    tcx,
                    body,
                    bb,
                    block_data,
                    stmt_idx,
                    stmt,
                    &mut insert_points,
                    &mut byte_copy_src_for_local,
                    &mut ssa_anchor_for_expr,
                    &mut bb_projected_reborrow_anchor_specs,
                    &mut ptr_locals_needing_tag,
                    &mut tagged_ptr_locals,
                    &mut boundary_recovered_ptr_locals,
                    &ptr_locals_with_tag_sources,
                    &summary_elidable_shared_call_ref_locals,
                    &interesting_stack_locals,
                    track_all_stack_allocs,
                    true,
                );
            }

            for (key, deps) in bb_projected_reborrow_anchor_specs {
                projected_reborrow_anchor_specs.entry(key).or_insert(deps);
            }

            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    ..
                } = &term.kind
                {
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
                        &mut boundary_recovered_ptr_locals,
                        &mut tagged_ptr_locals,
                        &mut projectionless_anchor_suppressed_locals,
                        &interesting_stack_locals,
                        &mut local_slot_shadow_store_locals,
                        &local_ref_use_stats,
                    );
                    self.invalidate_ssa_anchors_for_call(
                        body,
                        &mut ssa_anchor_for_expr,
                        args,
                        destination,
                        true,
                    );
                }

                if let TerminatorKind::Drop { place, .. } = &term.kind {
                    self.scan_drop_terminator(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        *place,
                        &mut insert_points,
                        &mut ptr_locals_needing_tag,
                        &boundary_recovered_ptr_locals,
                        &mut tagged_ptr_locals,
                        &interesting_stack_locals,
                    );
                }

                if let TerminatorKind::Return = &term.kind {
                    let callee_id = self.callee_id_u64(tcx, body.source.def_id());
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
                            kind: InstrKind::RetPush {
                                callee_id,
                                ptr_local: RETURN_PLACE,
                            },
                        });
                    } else if self.ty_contains_direct_pointer_fields(tcx, body, body.return_ty()) {
                        let leaf_ptrs = self.shadowable_leaf_ptr_places_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            body.return_ty(),
                        );
                        if leaf_ptrs.is_empty() {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(RETURN_PLACE),
                                kind: InstrKind::RetValidate {
                                    callee_id,
                                    local: RETURN_PLACE,
                                },
                            });
                        }
                        for leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            body.return_ty(),
                        ) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: leaf_spec.place,
                                kind: InstrKind::RetLeafPush {
                                    callee_id,
                                    leaf_key: leaf_spec.transport_key(),
                                },
                            });
                        }
                    }
                    for (arg_index, arg_local) in body.args_iter().enumerate() {
                        let arg_ty = body.local_decls[arg_local].ty;
                        let TyKind::Ref(_, pointee_ty, mutbl) = arg_ty.kind() else {
                            continue;
                        };
                        if !matches!(mutbl, Mutability::Mut) || self.is_pointer_ty(*pointee_ty) {
                            continue;
                        }
                        let pointee_ty = tcx
                            .try_normalize_erasing_regions(body.typing_env(tcx), *pointee_ty)
                            .unwrap_or(*pointee_ty);
                        if !pointee_ty.is_sized(tcx, body.typing_env(tcx)) {
                            continue;
                        }
                        if !self.ty_contains_pointer_fields(tcx, body, pointee_ty, 4) {
                            continue;
                        }
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(arg_local),
                            kind: InstrKind::MutArgRetPush {
                                callee_id,
                                arg_index: arg_index as u64,
                                ptr_local: arg_local,
                            },
                        });
                        for leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(arg_local).project_deeper(&[PlaceElem::Deref], tcx),
                            pointee_ty,
                        ) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: leaf_spec.place,
                                kind: InstrKind::MutArgRetLeafPush {
                                    callee_id,
                                    arg_index: arg_index as u64,
                                    ptr_local: arg_local,
                                    leaf_key: leaf_spec.transport_key(),
                                },
                            });
                        }
                    }
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(RETURN_PLACE),
                        kind: InstrKind::FnExit { callee_id },
                    });
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
        let mut fallback_return_locals: Vec<(Local, SizeOperand<'tcx>)> = Vec::new();

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
            self.trace_stack_alloc_emit(tcx, body, local, true, &size_op, "FallbackEntry");

            if !return_sites.is_empty() {
                fallback_return_locals.push((local, size_op.clone()));
            }
        }

        // Prepend entry fallback points so they are applied last and execute first.
        insert_points.splice(0..0, fallback_entry_points);

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
            local_slot_shadow_store_locals,
            projected_reborrow_anchor_specs,
            projectionless_anchor_suppressed_locals,
            interesting_stack_locals,
            fallback_return_locals,
            return_sites,
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

    fn allocate_u8_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        ptrs: HashSet<Local>,
    ) -> HashMap<Local, Local> {
        let mut local_for_ptr_local: HashMap<Local, Local> = HashMap::new();
        for ptr_local in ptrs.into_iter() {
            if !local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u8, rustc_span::DUMMY_SP));
                local_for_ptr_local.insert(ptr_local, t);
            }
        }
        local_for_ptr_local
    }

    fn allocate_reborrow_anchor_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        anchor_specs: &ReborrowAnchorSpecMap,
    ) -> HashMap<String, Local> {
        let mut anchor_local_for_key: HashMap<String, Local> = HashMap::new();
        let mut keys: Vec<&String> = anchor_specs.keys().collect();
        keys.sort();
        for key in keys {
            let local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
            anchor_local_for_key.insert(key.clone(), local);
        }
        anchor_local_for_key
    }

    fn ptr_state_locals_for_ptr_local(
        &self,
        ptr_local: Local,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
    ) -> Option<PtrStateLocals> {
        Some(PtrStateLocals {
            tag_local: tag_local_for_ptr_local.get(&ptr_local).copied()?,
            ref_ancestor_local: ref_ancestor_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_parent_local: export_parent_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_recovered_local: export_parent_is_recovered_local_for_ptr_local
                .get(&ptr_local)
                .copied(),
        })
    }

    fn carrier_slot_locals_for_local(
        &self,
        local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<CarrierSlotLocals> {
        Some(CarrierSlotLocals {
            anchor_local: reborrow_anchor_local_for_stack_local.get(&local).copied()?,
            slot_family_valid_local: anchor_is_slot_family_local_for_stack_local
                .get(&local)
                .copied(),
        })
    }

    fn schedule_reborrow_anchor_resets<'tcx>(
        &self,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        reborrow_anchor_specs: &ReborrowAnchorSpecMap,
        reborrow_anchor_local_for_key: &HashMap<String, Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let source_info = stmt.source_info;
                let touched_local = match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        Some(local)
                    }
                    StatementKind::Assign(box (place, _)) => place.as_local(),
                    _ => None,
                };
                if let Some(local) = touched_local {
                    if let Some(slot_state) = self.carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info,
                            place: Place::from(slot_state.anchor_local),
                            kind: InstrKind::ReborrowAnchorZero {
                                anchor_local: slot_state.anchor_local,
                                anchor_state_local: slot_state.slot_family_valid_local,
                            },
                        });
                    }
                    for (key, deps) in reborrow_anchor_specs.iter() {
                        if deps.contains(&local) {
                            if let Some(anchor_local) =
                                reborrow_anchor_local_for_key.get(key).copied()
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info,
                                    place: Place::from(anchor_local),
                                    kind: InstrKind::ReborrowAnchorZero {
                                        anchor_local,
                                        anchor_state_local: None,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    fn schedule_reborrow_anchor_propagation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let Some(dst_slot_state) = self.carrier_slot_locals_for_local(
                    dst_local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };
                let src_place = match rvalue {
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place)) => *src_place,
                    _ => continue,
                };
                let src_local = src_place.local;
                if src_local == dst_local {
                    continue;
                }
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty)
                    && !src_place.projection.is_empty()
                    && self.supports_slot_family_local(tcx, body, dst_local)
                {
                    if let Some(recovered_src_local) = self
                        .backtrack_same_typed_call_result_source_local(tcx, body, src_local, dst_ty)
                    {
                        if recovered_src_local != src_local {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                            continue;
                        }
                    }
                }
                if !reborrow_anchor_local_for_stack_local.contains_key(&src_local) {
                    let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx, body, src_local, dst_ty,
                            )
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                            continue;
                        }
                    }
                    if src_ty == dst_ty
                        && self.is_pointer_ty(body.local_decls[src_local].ty)
                        && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::ReborrowAnchorSet {
                                dst_local,
                                anchor_local: dst_slot_state.anchor_local,
                                anchor_state_local: dst_slot_state.slot_family_valid_local,
                                src_ptr_local: src_local,
                            },
                        });
                    }
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::ReborrowAnchorSeed {
                        dst_local,
                        src_local,
                        mark_slot_family: !src_place.projection.is_empty()
                            && self.supports_call_boundary_anchor_local(tcx, body, dst_local),
                    },
                });
            }
        }
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
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));

            let zero: Operand<'tcx> = self.const_u64(tcx, source_info.span, 0);
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((Place::from(*tag_local), Rvalue::Use(zero)))),
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

    fn init_extra_tag_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        tag_locals: &[Local],
    ) {
        if tag_locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for tag_local in tag_locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*tag_local),
                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                ))),
            ));
        }

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

    fn init_extra_u8_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        locals: &[Local],
    ) {
        if locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for local in locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*local),
                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                ))),
            ));
        }

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

    /// Add holder acquire/release hooks around hidden tag-local writes after the main MIR
    /// instrumentation pass has materialized them.
    fn schedule_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        manual_holder_managed_tag_assignments: &HashSet<(BasicBlock, usize)>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        let mut ptr_local_for_tag_local: HashMap<Local, Local> = HashMap::new();
        for (ptr_local, tag_local) in tag_local_for_ptr_local {
            ptr_local_for_tag_local.insert(*tag_local, *ptr_local);
        }

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                match stmt.kind {
                    StatementKind::Assign(box (dst_place, _)) => {
                        if manual_holder_managed_tag_assignments.contains(&(bb, stmt_idx)) {
                            continue;
                        }
                        let Some(dst_local) = dst_place.as_local() else {
                            continue;
                        };
                        let Some(ptr_local) = ptr_local_for_tag_local.get(&dst_local).copied()
                        else {
                            continue;
                        };
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagLocalKill {
                                tag_local: dst_local,
                            },
                        });
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                    StatementKind::StorageDead(dead_local) => {
                        if !tag_local_for_ptr_local.contains_key(&dead_local) {
                            continue;
                        }
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(dead_local),
                            kind: InstrKind::TagKill {
                                ptr_local: dead_local,
                            },
                        });
                    }
                    _ => {}
                }
            }

            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            let TerminatorKind::Call {
                destination,
                target,
                ..
            } = &term.kind
            else {
                continue;
            };
            if let Some(target_bb) = *target {
                if let Some(dst_local) = destination.as_local() {
                    if ptr_local_for_tag_local.contains_key(&dst_local) {
                        insert_points.push(InsertPoint {
                            bb: target_bb,
                            stmt_idx: 0,
                            insert_before: false,
                            source_info: term.source_info,
                            place: *destination,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                }
            }
        }
    }

    fn schedule_aux_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        aux_tag_locals: &HashSet<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, _)) = stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                if !aux_tag_locals.contains(&dst_local) {
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: true,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagLocalKill {
                        tag_local: dst_local,
                    },
                });
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagRetain {
                        tag_local: dst_local,
                    },
                });
            }
        }
    }

    fn func_operand_for<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        hooks: Hooks,
        kind: &InstrKind<'tcx>,
        sp: Span,
    ) -> Operand<'tcx> {
        let def_id = match kind {
            InstrKind::Ref { .. } => hooks.def_id_ref,
            InstrKind::DebugRefActivate { .. } => hooks.def_id_debug_ref,
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
            InstrKind::StackSlotWriteAllowUntagged { .. } => {
                hooks.def_id_local_write_allow_untagged
            }
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrReadAllowUntagged { .. } => hooks.def_id_read_allow_untagged,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::ShadowLoad { .. } => hooks.def_id_shadow_load_tag,
            InstrKind::ShadowStore { .. } => hooks.def_id_shadow_store_ptr,
            InstrKind::ShadowStoreBoxPointee { .. } => hooks.def_id_shadow_store_ptr,
            InstrKind::ShadowCopySlot { .. } => hooks.def_id_shadow_copy_slot,
            InstrKind::ShadowCopyRange { .. } => hooks.def_id_shadow_copy_range,
            InstrKind::ShadowKill { .. } => hooks.def_id_shadow_kill_range,
            InstrKind::TagKill { .. } => hooks.def_id_tag_kill,
            InstrKind::TagLocalKill { .. } => hooks.def_id_tag_kill,
            InstrKind::TagRetain { .. } => hooks.def_id_tag_retain,
            InstrKind::TagProp { .. } | InstrKind::TagPropFromRefAncestor { .. } => {
                hooks.def_id_use // should never become a call (handled as a plain Assign)
            }
            InstrKind::ReborrowAnchorZero { .. }
            | InstrKind::ReborrowAnchorSet { .. }
            | InstrKind::ReborrowAnchorSeed { .. }
            | InstrKind::ParentTagSnapshot { .. } => {
                hooks.def_id_use // should never become a call (handled as a plain Assign)
            }
            InstrKind::PtrDerive { is_ref, .. } | InstrKind::PtrDeriveParent { is_ref, .. } => {
                if *is_ref {
                    hooks.def_id_ref
                } else {
                    hooks.def_id_raw
                }
            }
            InstrKind::CallArgPush { .. } => hooks.def_id_push_call_arg_tag,
            InstrKind::CallArgValidate { .. } => hooks.def_id_validate_call_arg_tag,
            InstrKind::ArgRetag { .. } | InstrKind::ArgAnchorTake { .. } => {
                hooks.def_id_take_call_arg_tag
            }
            InstrKind::RetValidate { .. } => hooks.def_id_validate_ret_tag,
            InstrKind::RetLeafPush { .. } => hooks.def_id_push_ret_leaf_shadow,
            InstrKind::RetPush { .. } => hooks.def_id_push_ret_tag,
            InstrKind::RetAnchorTake { .. } => hooks.def_id_take_ret_tag,
            InstrKind::RetLeafTake { .. } => hooks.def_id_take_ret_leaf_shadow,
            InstrKind::RetAnchorRoot { .. } => hooks.def_id_raw,
            InstrKind::RetTake { .. } => hooks.def_id_take_ret_tag_or_root,
            InstrKind::MutArgRetPush { .. } => hooks.def_id_push_mut_arg_ret_tag,
            InstrKind::MutArgRetLeafPush { .. } => hooks.def_id_push_mut_arg_ret_leaf_shadow,
            InstrKind::MutArgRetTake { .. } => hooks.def_id_take_mut_arg_ret_tag,
            InstrKind::MutArgRetTakePtrOnly { .. } => hooks.def_id_take_mut_arg_ret_tag_or_zero,
            InstrKind::MutArgRetLeafTake { .. } => hooks.def_id_take_mut_arg_ret_leaf_shadow,
            InstrKind::FnExit { .. } => hooks.def_id_exit_fn,
        };
        Operand::function_handle(tcx, def_id, std::iter::empty(), sp)
    }

    fn insert_instrumentation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        insert_points: Vec<InsertPoint<'tcx>>,
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        fn resolve_return_chain_bb<'tcx>(body: &Body<'tcx>, mut bb: BasicBlock) -> BasicBlock {
            for _ in 0..64 {
                let Some(term) = body.basic_blocks[bb].terminator.as_ref() else {
                    break;
                };
                match &term.kind {
                    TerminatorKind::Return => break,
                    // End-of-block instrumentation on a return site can be applied after the
                    // block has already been split into a synthetic call chain. Follow the
                    // continuation so return-side hooks still land immediately before the
                    // real `Return`.
                    TerminatorKind::Call {
                        target: Some(next), ..
                    } if body.basic_blocks[bb].statements.is_empty() => {
                        bb = *next;
                    }
                    _ => break,
                }
            }
            bb
        }

        fn is_runtime_hook_call<'tcx>(
            tcx: TyCtxt<'tcx>,
            term: &Terminator<'tcx>,
        ) -> Option<BasicBlock> {
            let TerminatorKind::Call {
                func,
                target: Some(next),
                ..
            } = &term.kind
            else {
                return None;
            };

            let Operand::Constant(c) = func else {
                return None;
            };
            let TyKind::FnDef(def_id, _) = c.const_.ty().kind() else {
                return None;
            };
            (tcx.crate_name(def_id.krate).as_str() == "runtime").then_some(*next)
        }

        fn resolve_split_chain_insert_site<'tcx>(
            tcx: TyCtxt<'tcx>,
            body: &Body<'tcx>,
            orig_stmt_prefix_len: &HashMap<BasicBlock, usize>,
            mut bb: BasicBlock,
            mut stmt_idx: usize,
            insert_before: bool,
        ) -> (BasicBlock, usize) {
            for _ in 0..64 {
                let bd = &body.basic_blocks[bb];
                let len = orig_stmt_prefix_len
                    .get(&bb)
                    .copied()
                    .unwrap_or_else(|| bd.statements.len());
                let needs_follow = if insert_before {
                    stmt_idx > len
                } else {
                    stmt_idx >= len
                };
                if !needs_follow {
                    break;
                }
                let Some(term) = bd.terminator.as_ref() else {
                    break;
                };
                let Some(next) = is_runtime_hook_call(tcx, term) else {
                    break;
                };
                stmt_idx = stmt_idx.saturating_sub(len);
                bb = next;
            }
            (bb, stmt_idx)
        }

        fn instr_priority(kind: &InstrKind<'_>) -> u8 {
            match kind {
                InstrKind::Ref { .. }
                | InstrKind::Raw { .. }
                | InstrKind::RawRoot { .. }
                | InstrKind::ArgRetag { .. }
                | InstrKind::ArgAnchorTake { .. }
                | InstrKind::RetRoot { .. }
                | InstrKind::PtrDerive { .. }
                | InstrKind::PtrDeriveParent { .. } => 0,
                // Tag propagation must execute after tag-creating hooks but before
                // access/usage hooks at the same insertion site.
                InstrKind::TagProp { .. }
                | InstrKind::TagPropFromRefAncestor { .. }
                | InstrKind::FnExit { .. }
                | InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. }
                | InstrKind::ReborrowAnchorSet { .. }
                | InstrKind::ReborrowAnchorSeed { .. }
                | InstrKind::ReborrowAnchorZero { .. }
                | InstrKind::ParentTagSnapshot { .. } => 1,
                InstrKind::ShadowLoad { .. } => 1,
                // Debug ref activation may need the restored tag/ref_ancestor emitted by
                // ShadowLoad or TagProp at the same definition site.
                InstrKind::DebugRefActivate { .. } => 2,
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. }
                // Return-boundary exports must run before FnExit tears down the callee frame.
                | InstrKind::RetPush { .. }
                | InstrKind::RetLeafPush { .. }
                | InstrKind::RetAnchorRoot { .. }
                | InstrKind::MutArgRetPush { .. }
                | InstrKind::MutArgRetLeafPush { .. }
                | InstrKind::ShadowKill { .. } => 2,
                // Keep stack-slot writes in the same bucket as reborrow-anchor maintenance so
                // the later-scheduled anchor zeroing is inserted first and then moved after the
                // write call when we split the block at this statement.
                InstrKind::StackSlotWriteAllowUntagged { .. } => 1,
                InstrKind::ShadowStore { .. }
                | InstrKind::ShadowStoreBoxPointee { .. }
                | InstrKind::ShadowCopySlot { .. }
                | InstrKind::ShadowCopyRange { .. } => 3,
                InstrKind::CallArgPush { .. }
                | InstrKind::CallArgValidate { .. }
                | InstrKind::PtrUse { .. }
                | InstrKind::RetValidate { .. }
                | InstrKind::RetAnchorTake { .. }
                | InstrKind::RetLeafTake { .. }
                | InstrKind::MutArgRetTake { .. }
                | InstrKind::MutArgRetLeafTake { .. }
                | InstrKind::MutArgRetTakePtrOnly { .. } => 3,
                _ => 4,
            }
        }

        // Entry-side arg retag/anchor initialization must run before any ptr reads/writes.
        // We split it out so we can enforce ordering independent of stmt_idx sorting.
        let mut arg_retag_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();
        let mut other_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();

        for (idx, ip) in insert_points.into_iter().enumerate() {
            if matches!(
                ip.kind,
                InstrKind::ArgRetag { .. } | InstrKind::ArgAnchorTake { .. }
            ) {
                arg_retag_points.push((idx, ip));
            } else {
                other_points.push((idx, ip));
            }
        }

        let sort_points = |points: &mut Vec<(usize, InsertPoint<'tcx>)>| {
            points.sort_by_key(|(idx, ip)| {
                (ip.bb.index(), ip.stmt_idx, instr_priority(&ip.kind), *idx)
            });
        };

        sort_points(&mut other_points);
        sort_points(&mut arg_retag_points);

        let mut orig_stmt_prefix_len: HashMap<BasicBlock, usize> = HashMap::new();

        for (_idx, ip) in other_points.into_iter().rev() {
            let mut bb = ip.bb;
            let mut stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            let (resolved_bb, resolved_stmt_idx) = resolve_split_chain_insert_site(
                tcx,
                body,
                &orig_stmt_prefix_len,
                bb,
                stmt_idx,
                ip.insert_before,
            );
            bb = resolved_bb;
            stmt_idx = resolved_stmt_idx;

            if matches!(creation_kind, InstrKind::StackAlloc { live: false, .. })
                && !ip.insert_before
            {
                let resolved_bb = resolve_return_chain_bb(body, bb);
                if resolved_bb != bb {
                    bb = resolved_bb;
                    stmt_idx = body.basic_blocks[bb].statements.len();
                }
            }

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

            if let InstrKind::ShadowLoad {
                dst_local,
                require_tag,
                validate_ref,
            } = creation_kind.clone()
            {
                let Some(dst_ptr_state) = self.ptr_state_locals_for_ptr_local(
                    dst_local,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    export_parent_local_for_ptr_local,
                    export_parent_is_recovered_local_for_ptr_local,
                ) else {
                    continue;
                };
                let Some(dst_ref_ancestor_local) = dst_ptr_state.ref_ancestor_local else {
                    continue;
                };
                let dst_export_parent_local = dst_ptr_state.boundary_parent_local;
                let dst_recovered_local = dst_ptr_state.boundary_recovered_local;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let ref_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                let needs_loaded_ref_validation =
                    matches!(body.local_decls[dst_local].ty.kind(), TyKind::Ref(..))
                        && (require_tag || validate_ref);
                let projected_field_ty = place.ty(&body.local_decls, tcx).ty;
                let projected_carrier_anchor = if !place.projection.is_empty()
                    && matches!(projected_field_ty.kind(), TyKind::Ref(..))
                    && !self.is_pointer_ty(body.local_decls[place.local].ty)
                {
                    reborrow_anchor_local_for_stack_local
                        .get(&place.local)
                        .copied()
                } else {
                    None
                };
                let post_load_block = if projected_carrier_anchor.is_some() {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else if needs_loaded_ref_validation {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else {
                    cont_block
                };
                let validate_block = if needs_loaded_ref_validation {
                    if projected_carrier_anchor.is_some() {
                        Some(
                            body.basic_blocks_mut()
                                .push(BasicBlockData::new(None, is_cleanup)),
                        )
                    } else {
                        Some(post_load_block)
                    }
                } else {
                    None
                };

                let tag_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let tag_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(addr_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let ref_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_ref_ancestor,
                    std::iter::empty(),
                    source_info.span,
                );
                let ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(addr_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let export_parent_func = dst_export_parent_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let export_parent_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(addr_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let recovered_func = dst_recovered_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent_recovered,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let recovered_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(addr_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let final_post_load_block = post_load_block;
                let recovered_block = dst_recovered_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let export_parent_target = recovered_block.unwrap_or(final_post_load_block);
                let export_parent_block = dst_export_parent_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let ref_target = export_parent_block.unwrap_or(export_parent_target);

                body.basic_blocks_mut()[ref_block].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: ref_func,
                        args: ref_args,
                        destination: Place::from(dst_ref_ancestor_local),
                        target: Some(ref_target),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });

                if let (Some(export_parent_block), Some(export_parent_local), Some(export_parent_func)) =
                    (export_parent_block, dst_export_parent_local, export_parent_func)
                {
                    body.basic_blocks_mut()[export_parent_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: export_parent_func,
                            args: export_parent_args,
                            destination: Place::from(export_parent_local),
                            target: Some(export_parent_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let (Some(recovered_block), Some(recovered_local), Some(recovered_func)) =
                    (recovered_block, dst_recovered_local, recovered_func)
                {
                    body.basic_blocks_mut()[recovered_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: recovered_func,
                            args: recovered_args,
                            destination: Place::from(recovered_local),
                            target: Some(final_post_load_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let Some(anchor_local) = projected_carrier_anchor {
                    let tag_is_zero_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                    let validate_target = validate_block.unwrap_or(cont_block);
                    let repair_cont_block = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: validate_target,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[repair_cont_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_local))),
                            ))),
                        ));
                    if let Some(dst_export_parent_local) = dst_export_parent_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        dst_ptr_state.tag_local,
                                    ))),
                                ))),
                            ));
                    }
                    if let Some(dst_recovered_local) = dst_recovered_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                                ))),
                            ));
                    }

                    let repair_call_block = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup));
                    let repair_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (repair_addr_stmt1_opt, repair_addr_stmt2) = self
                        .addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(dst_local),
                            repair_addr_local,
                        )
                        .expect("ShadowLoad projected ref repair on non-pointer local");
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    let dst_ty = body.local_decls[dst_local].ty;
                    let is_mut_u8 = match dst_ty.kind() {
                        TyKind::Ref(_, _, Mutability::Mut) => 1,
                        _ => 0,
                    };
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    // Missing field-slot shadow on projected carrier refs should be repaired
                    // by rebuilding a ref tag from the loaded pointer value, not by copying the
                    // outer carrier anchor into the local tag directly.
                    alias_flags |= 0b10;
                    let repair_ref_func = Operand::function_handle(
                        tcx,
                        hooks.def_id_ref,
                        std::iter::empty(),
                        source_info.span,
                    );
                    let repair_ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(repair_addr_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, is_mut_u8),
                            span: source_info.span,
                        },
                        Spanned {
                            node: Operand::Copy(Place::from(anchor_local)),
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
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    let repair_bd = &mut body.basic_blocks_mut()[repair_call_block];
                    if let Some(stmt) = repair_addr_stmt1_opt {
                        repair_bd.statements.push(stmt);
                    }
                    repair_bd.statements.push(repair_addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        repair_bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        repair_bd.statements.append(&mut align_stmts);
                    }
                    repair_bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: repair_ref_func,
                            args: repair_ref_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(repair_cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });

                    body.basic_blocks_mut()[post_load_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tag_is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ));
                    body.basic_blocks_mut()[post_load_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::SwitchInt {
                            discr: Operand::Copy(Place::from(tag_is_zero_local)),
                            targets: SwitchTargets::static_if(
                                0,
                                validate_target,
                                repair_call_block,
                            ),
                        },
                    });
                }

                if let Some(validate_block) = validate_block {
                    let validate_func = Operand::function_handle(
                        tcx,
                        if require_tag {
                            hooks.def_id_require_loaded_ptr_tag
                        } else {
                            hooks.def_id_validate_loaded_ref_tag
                        },
                        std::iter::empty(),
                        source_info.span,
                    );
                    let validate_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    body.basic_blocks_mut()[validate_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: validate_func,
                            args: validate_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(slot_addr_stmt1);
                    bd.statements.push(slot_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: tag_func,
                            args: tag_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(ref_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopySlot { src_place } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_slot,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopyRange { src_place, size_op } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let (arg_size, mut size_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &size_op);
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_range,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_size,
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.statements.append(&mut size_stmts);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowStoreBoxPointee {
                box_local,
                src_local,
            } = creation_kind.clone()
            {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self
                    .box_pointee_slot_addr_stmts_for_local(
                        tcx,
                        body,
                        source_info,
                        box_local,
                        dst_addr_local,
                    )
                else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let tag_op: Operand<'tcx> =
                    if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let ref_ancestor_op: Operand<'tcx> =
                    if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let export_parent_op: Operand<'tcx> = if let Some(tl) =
                    export_parent_local_for_ptr_local.get(&src_local)
                {
                    Operand::Copy(Place::from(*tl))
                } else {
                    tag_op.clone()
                };
                let recovered_op: Operand<'tcx> = if let Some(tl) =
                    export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                {
                    Operand::Copy(Place::from(*tl))
                } else {
                    self.const_u8(tcx, source_info.span, 0)
                };
                let store_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_store_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let store_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: ref_ancestor_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: export_parent_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: recovered_op,
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: store_func,
                            args: store_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            // workaround for pointers produced from NonNull/Unique via Transmute
            // RawRoot lowering: we implement this by mirroring the existing Raw lowering code path:
            //   tag(ptr_local) = __record_raw_ptr_creation(expose(ptr_local), is_mut, 0)
            if let InstrKind::RawRoot {
                ptr_local,
                is_mut,
                exposed_provenance,
            } = creation_kind.clone()
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if !self.is_pointer_ty(ptr_ty) {
                    continue;
                }
                let raw_root_is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
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
                let call_bb = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto { target: call_bb },
                });

                if let Some(data_ptr_stmt) = data_ptr_stmt_opt {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .push(data_ptr_stmt);
                }
                body.basic_blocks_mut()[call_bb].statements.push(addr_stmt);
                if !bounds_len_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut bounds_len_stmts);
                }
                if !align_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut align_stmts);
                }

                let root_func = Operand::function_handle(
                    tcx,
                    if raw_root_is_ref {
                        hooks.def_id_ref
                    } else {
                        hooks.def_id_raw
                    },
                    std::iter::empty(),
                    source_info.span,
                );
                let args_root: Box<[Spanned<Operand<'tcx>>]> = vec![
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
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            (if alias_exempt { 1 } else { 0 })
                                | (if exposed_provenance { 0b10_0000 } else { 0 }),
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[call_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: root_func,
                        args: args_root,
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
                                Rvalue::Use(if raw_root_is_ref {
                                    Operand::Copy(Place::from(dst_tag))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }),
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
            if let InstrKind::MutArgRetTake {
                callee_id,
                arg_index,
                local,
                ptr_local,
            } = creation_kind
            {
                let Some(slot_state) = self.carrier_slot_locals_for_local(
                    local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("MutArgRetTake on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag_or_zero,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                }

                body.basic_blocks_mut()[ret_take_cont_bb].statements.splice(
                    0..0,
                    [
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_u64_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(is_zero_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(nonzero_u64_local),
                                Rvalue::BinaryOp(
                                    BinOp::Sub,
                                    Box::new((
                                        self.const_u64(tcx, source_info.span, 1),
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(keep_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                        Operand::Copy(Place::from(slot_state.anchor_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(new_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(nonzero_u64_local)),
                                        Operand::Copy(Place::from(dst_tag_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(selected_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(keep_part_local)),
                                        Operand::Copy(Place::from(new_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(slot_state.anchor_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ),
                    ],
                );
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        7,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            if let InstrKind::RetTake {
                callee_id,
                dst_local,
            } = creation_kind
            {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing tag local for RetTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
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
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
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
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
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
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
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
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&dst_local)
                    .copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetAnchorTake { callee_id, local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_tag,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetLeafTake {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            if let InstrKind::RetAnchorRoot { local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorRoot");
                let local_ty = body.local_decls[local].ty;
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorRoot");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("RetAnchorRoot on unsupported local");
                let align_op = self.align_operand_for_ty(tcx, body, local_ty, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_raw,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
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
                                node: self.const_u8(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: arg_align,
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::MutArgRetLeafTake {
                callee_id,
                arg_index,
                local,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (base_addr_stmt1, base_addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        base_addr_local,
                        false,
                    )
                    .expect("MutArgRetLeafTake on unsupported local");
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_mut_arg_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(base_addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(slot_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(base_addr_stmt1);
                    take_bd.statements.push(base_addr_stmt2);
                    take_bd.statements.push(slot_addr_stmt1);
                    take_bd.statements.push(slot_addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            // Caller-side pointer-only writeback:
            // forwarders that receive `&mut T` directly do not have a separate carrier local in
            // this frame, but the live pointer local still needs the post-call family before any
            // later use or re-export.
            if let InstrKind::MutArgRetTakePtrOnly {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTakePtrOnly");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("MutArgRetTakePtrOnly on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
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
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Eq,
                                        Box::new((
                                            Operand::Copy(Place::from(dst_tag_local)),
                                            self.const_u64(tcx, source_info.span, 0),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_u64_local),
                                    Rvalue::Cast(
                                        CastKind::IntToInt,
                                        Operand::Copy(Place::from(is_zero_local)),
                                        tcx.types.u64,
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(nonzero_u64_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Sub,
                                        Box::new((
                                            self.const_u64(tcx, source_info.span, 1),
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_new_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(nonzero_u64_local)),
                                            Operand::Copy(Place::from(dst_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(ptr_new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::MutArgRetPush {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for MutArgRetPush");

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
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
                bd.statements.push(addr_stmt1);
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::MutArgRetLeafPush {
                callee_id,
                arg_index,
                ptr_local,
                leaf_key,
            } = creation_kind
            {
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((base_addr_stmt1, base_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    base_addr_local,
                    false,
                ) else {
                    continue;
                };
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(base_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(slot_addr_local)),
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
                bd.statements.push(base_addr_stmt1);
                bd.statements.push(base_addr_stmt2);
                bd.statements.push(slot_addr_stmt1);
                bd.statements.push(slot_addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::RetPush {
                callee_id,
                ptr_local,
            } = creation_kind
            {
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
                let selected_tag_local = if let (Some(export_parent_local), Some(recovered_local)) = (
                    export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied(),
                ) {
                    let recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let not_recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let fallback_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let export_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let selected_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let bd = &mut body.basic_blocks_mut()[bb];
                    bd.statements.extend([
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_u64_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(recovered_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(not_recovered_u64_local),
                                Rvalue::BinaryOp(
                                    BinOp::Sub,
                                    Box::new((
                                        self.const_u64(tcx, source_info.span, 1),
                                        Operand::Copy(Place::from(recovered_u64_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(fallback_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(not_recovered_u64_local)),
                                        Operand::Copy(Place::from(tag_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(recovered_u64_local)),
                                        Operand::Copy(Place::from(export_parent_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(selected_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(fallback_part_local)),
                                        Operand::Copy(Place::from(export_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                    ]);
                    Some(selected_local)
                } else {
                    None
                };

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
                        node: Operand::Copy(Place::from(selected_tag_local.unwrap_or(tag_local))),
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

            if let InstrKind::RetValidate { callee_id, local } = creation_kind {
                let tag_op = self.parent_tag_operand_for_src_place(
                    tcx,
                    body,
                    bb,
                    stmt_idx,
                    source_info,
                    Place::from(local),
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    reborrow_anchor_local_for_stack_local,
                    projectionless_anchor_suppressed_locals,
                    false,
                    true,
                    ParentSelectionMode::PointeeFamily,
                );
                let validate_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_validate_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_validate: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
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
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: validate_func,
                        args: args_validate,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
                continue;
            }

            if let InstrKind::RetLeafPush {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
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
                bd.statements.push(addr_stmt1);
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
                let prop_stmt = if copy_tag {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for TagProp dst");

                    let src_op: Operand<'tcx> =
                        if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
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
                    let src_ref_ancestor_op: Operand<'tcx> = if let Some(src_ref_ancestor) =
                        ref_ancestor_local_for_ptr_local.get(&src)
                    {
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
                    next_insert += 1;
                }
                if copy_tag {
                    if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                        export_parent_local_for_ptr_local.get(&dst).copied(),
                        export_parent_local_for_ptr_local.get(&src).copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        src_export_parent_local,
                                    ))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                    if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&dst)
                            .copied(),
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&src)
                            .copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                }
                continue;
            }

            if let InstrKind::TagPropFromRefAncestor { dst, src } = creation_kind {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst)
                    .expect("missing tag local for TagPropFromRefAncestor dst");
                let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                    .get(&dst)
                    .expect("missing ref-ancestor local for TagPropFromRefAncestor dst");
                let src_ref_ancestor_op: Operand<'tcx> =
                    if let Some(src_ref_ancestor) = ref_ancestor_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*src_ref_ancestor))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                let tag_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_tag),
                        Rvalue::Use(src_ref_ancestor_op.clone()),
                    ))),
                );
                let ref_ancestor_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_ref_ancestor),
                        Rvalue::Use(src_ref_ancestor_op),
                    ))),
                );

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut prop_stmts = vec![tag_stmt, ref_ancestor_stmt];
                if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                    export_parent_local_for_ptr_local.get(&dst).copied(),
                    export_parent_local_for_ptr_local.get(&src).copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_export_parent_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_export_parent_local))),
                        ))),
                    ));
                }
                if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&dst)
                        .copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&src)
                        .copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_recovered_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, prop_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorZero {
                anchor_local,
                anchor_state_local,
            } = creation_kind
            {
                let mut zero_stmts = vec![Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                    ))),
                )];
                if let Some(anchor_state_local) = anchor_state_local {
                    zero_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.splice(insert_at..insert_at, zero_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSet {
                dst_local: _dst_local,
                anchor_local,
                anchor_state_local,
                src_ptr_local,
            } = creation_kind
            {
                let src_tag_operand = if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_ptr_local).copied()
                {
                    Operand::Copy(Place::from(src_tag_local))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_ptr_local)
                    .copied()
                {
                    Operand::Copy(Place::from(src_anchor_local))
                } else {
                    panic!("missing lineage source for ReborrowAnchorSet src");
                };
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let anchor_is_zero_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_is_zero_local),
                        Rvalue::BinaryOp(
                            BinOp::Eq,
                            Box::new((
                                Operand::Copy(Place::from(anchor_local)),
                                self.const_u64(tcx, source_info.span, 0),
                            )),
                        ),
                    ))),
                );
                let anchor_should_init_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_should_init_local),
                        Rvalue::Cast(
                            CastKind::IntToInt,
                            Operand::Copy(Place::from(anchor_is_zero_local)),
                            tcx.types.u64,
                        ),
                    ))),
                );
                let anchor_new_part_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_new_part_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(anchor_should_init_local)),
                                src_tag_operand,
                            )),
                        ),
                    ))),
                );
                let anchor_selected_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_selected_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(anchor_local)),
                                Operand::Copy(Place::from(anchor_new_part_local)),
                            )),
                        ),
                    ))),
                );
                let set_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut stmts = vec![
                    anchor_is_zero_stmt,
                    anchor_should_init_stmt,
                    anchor_new_part_stmt,
                    anchor_selected_stmt,
                    set_stmt,
                ];
                if let Some(anchor_state_local) = anchor_state_local {
                    stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSeed {
                dst_local,
                src_local,
                mark_slot_family,
            } = creation_kind
            {
                let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&dst_local)
                    .copied()
                else {
                    continue;
                };
                let Some(src_tag_operand) = (if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_local).copied()
                {
                    Some(Operand::Copy(Place::from(src_tag_local)))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_local)
                    .copied()
                {
                    Some(Operand::Copy(Place::from(src_anchor_local)))
                } else {
                    None
                }) else {
                    continue;
                };
                let dst_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&dst_local)
                    .copied();
                let src_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&src_local)
                    .copied();
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.splice(
                    insert_at..insert_at,
                    [
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(anchor_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_should_init_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(anchor_is_zero_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_new_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(anchor_should_init_local)),
                                        src_tag_operand,
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_selected_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(anchor_local)),
                                        Operand::Copy(Place::from(anchor_new_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                            ))),
                        ),
                    ],
                );
                if let Some(dst_anchor_state_local) = dst_anchor_state_local {
                    let state_stmt = if mark_slot_family {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        )
                    } else if let Some(src_anchor_state_local) = src_anchor_state_local {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(Operand::Copy(Place::from(src_anchor_state_local))),
                            ))),
                        )
                    } else {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                            ))),
                        )
                    };
                    bd.statements.insert(insert_at + 5, state_stmt);
                }
                continue;
            }

            if let InstrKind::ParentTagSnapshot {
                dst_local,
                src,
                is_raw_creation,
            } = creation_kind
            {
                let parent_local = *ref_ancestor_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing ref-ancestor local for ParentTagSnapshot dst");
                let parent_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(parent_local),
                        Rvalue::Use(self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            src,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            is_raw_creation,
                            true,
                            self.creation_parent_selection_mode_for_src_place(
                                tcx,
                                body,
                                src,
                                true,
                            ),
                        )),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.insert(insert_at, parent_stmt);
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
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

                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
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
                    Spanned {
                        node: arg_align,
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
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
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
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        // Shared refs at function entry are boundary-recovered values. Preserve
                        // the incoming boundary parent for later call/return transport.
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(
                                    tcx,
                                    source_info.span,
                                    recovered_value,
                                )),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&local)
                    .copied();
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("ArgAnchorTake on unsupported local");
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
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
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_call_arg_tag_anchor,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(anchor_local),
                        target: Some(cont_block),
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
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    bd.terminator = Some(take_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                if let Some(anchor_state_local) = anchor_state_local {
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }
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

            if let InstrKind::TagKill { ptr_local } = creation_kind {
                let Some(tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() else {
                    continue;
                };
                let mut tag_locals: Vec<Local> = vec![tag_local];
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    if export_parent_local != tag_local {
                        tag_locals.push(export_parent_local);
                    }
                }

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let mut next_target = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                for kill_local in tag_locals.into_iter().rev() {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let call_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_tag_kill,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![Spanned {
                                node: Operand::Copy(Place::from(kill_local)),
                                span: source_info.span,
                            }]
                            .into_boxed_slice(),
                            destination: Place::from(tmp_unit),
                            target: Some(next_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    next_target = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(Some(call_term), is_cleanup));
                }
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto {
                        target: next_target,
                    },
                });
                continue;
            }

            if let InstrKind::TagLocalKill { tag_local } = creation_kind {
                let kill_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_kill,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_kill: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
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
                        func: kill_func,
                        args: args_kill,
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

            if let InstrKind::TagRetain { tag_local } = creation_kind {
                let retain_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_retain,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_retain: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
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
                        func: retain_func,
                        args: args_retain,
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

            let mut func_operand =
                self.func_operand_for(tcx, hooks, &creation_kind, source_info.span);
            if let InstrKind::ShadowStore { src_local } = creation_kind {
                if self.shadow_store_uses_local_slot_store(
                    local_slot_shadow_store_locals,
                    place,
                    src_local,
                ) {
                    func_operand = Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_store_ptr_local,
                        std::iter::empty(),
                        source_info.span,
                    );
                }
            }

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
                InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => Some(
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
                    // Stack-slot tracking only needs the numeric address of a concrete local in
                    // this MIR body. Opaque/generic-but-sized locals (for example
                    // `impl RangeBounds<usize>` in optimized `bytes::Bytes::slice`) are still
                    // valid `&raw const local` sources even though `is_thin_ptr_ty` rejects them
                    // for generic pointer-value exposure.
                    if !local_ty.is_sized(tcx, body.typing_env(tcx)) {
                        continue;
                    }
                    let ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
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
                InstrKind::HeapAlloc { ptr_local, .. }
                | InstrKind::ConstAlloc { ptr_local, .. } => {
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
                        // materialize `&raw {const,mut} <full place>` and expose that pointer.
                        // Using only `place.local` here points at the base carrier and can
                        // turn valid projected accesses into false OOB/WILD reports.
                        let place_ty = place.ty(&body.local_decls, tcx).ty;
                        let is_write = matches!(
                            creation_kind,
                            InstrKind::PtrWrite { .. } | InstrKind::PtrWriteAllowUntagged { .. }
                        );
                        let raw_ptr_ty = if is_write {
                            Ty::new_mut_ptr(tcx, place_ty)
                        } else {
                            Ty::new_imm_ptr(tcx, place_ty)
                        };
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
                                Rvalue::RawPtr(
                                    if is_write {
                                        RawPtrKind::Mut
                                    } else {
                                        RawPtrKind::Const
                                    },
                                    place,
                                ),
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
                InstrKind::StackSlotWriteAllowUntagged { .. } => {
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        true,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::CallArgPush { ptr_local, .. } => {
                    let place_ty = place.ty(&body.local_decls, tcx).ty;
                    if self.is_pointer_ty(place_ty) {
                        match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                            Some(stmts) => stmts,
                            None => continue,
                        }
                    } else {
                        let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            place,
                            addr_local,
                            false,
                        ) else {
                            continue;
                        };
                        (Some(slot_stmt1), slot_stmt2)
                    }
                }
                InstrKind::CallArgValidate { .. } | InstrKind::RetValidate { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. } => {
                    let is_mut = matches!(
                        creation_kind,
                        InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. }
                    );
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        is_mut,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                _ => match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                    Some(stmts) => stmts,
                    None => continue,
                },
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

            let orig_stmt_len = orig_stmt_prefix_len
                .get(&bb)
                .copied()
                .unwrap_or_else(|| body.basic_blocks[bb].statements.len());

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc {
                    ptr_local, live, ..
                } => Some((*ptr_local, *live)),
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
            let projected_ref_parent_local = match &creation_kind {
                InstrKind::Ref {
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } if !src.projection.is_empty() => self
                    .materialize_projected_reborrow_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        projected_reborrow_anchor_key.as_ref(),
                        projected_reborrow_anchor_local_for_key,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    ),
                _ => None,
            };
            let projectionless_ref_parent_local = match &creation_kind {
                InstrKind::Ref { bk, src, .. }
                    if src.projection.is_empty()
                        && projectionless_anchor_suppressed_locals.contains(&src.local) =>
                {
                    self.materialize_projectionless_slot_anchor_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        *bk,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    )
                }
                _ => None,
            };

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrReadAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // Projected accesses like `(*self_ref).field`, `slice[i]`, or
                        // `(*wide_ref).0` must validate in the family of the accessed
                        // projection source, not blindly in the family of the coarse
                        // `ptr_local` chosen during scan. `PtrUse` already does this;
                        // keep read validation consistent to avoid reusing an outer
                        // `&Bytes` tag on the empty-slice sentinel pointer `0x1`.
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackAlloc {
                    ref size_op, live, ..
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    // Encode stack-alloc flag in bit1; bit0 is the live flag.
                    let live_bits: u8 = if live { 1 } else { 0 };
                    let arg_live = self.const_u8(tcx, source_info.span, live_bits | 0x2);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::HeapAlloc {
                    ptr_local: _,
                    live,
                    ref size_op,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
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
                    // __rz_record_alloc live bitfield:
                    // bit0 = live, bit1 = stack, bit2 = global/promoted const
                    let arg_live = self.const_u8(tcx, source_info.span, 0b101);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrWrite {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrWriteAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
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
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackSlotWriteAllowUntagged {
                    local, ref size_op, ..
                } => {
                    let Some(tag_local) =
                        reborrow_anchor_local_for_stack_local.get(&local).copied()
                    else {
                        continue;
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(tag_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgPush {
                    callee_id,
                    arg_index,
                    ptr_local,
                    flags,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let fallback_tag_op: Operand<'tcx> = if place.projection.is_empty()
                        && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                    {
                        let pointee_anchor_local = self
                            .backtrack_pointer_pointee_local(
                                body,
                                ptr_local,
                                &body.basic_blocks[bb].statements,
                            )
                            .and_then(|pointee_local| {
                                if self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                                    None
                                } else {
                                    reborrow_anchor_local_for_stack_local
                                        .get(&pointee_local)
                                        .copied()
                                }
                            });
                        if let Some(anchor_local) = pointee_anchor_local {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                let tag_is_zero_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                let tag_is_zero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let tag_is_nonzero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let keep_tag_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let anchor_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let selected_tag_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                extra_stmts.extend([
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Eq,
                                                Box::new((
                                                    Operand::Copy(Place::from(tl)),
                                                    self.const_u64(tcx, source_info.span, 0),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_u64_local),
                                            Rvalue::Cast(
                                                CastKind::IntToInt,
                                                Operand::Copy(Place::from(tag_is_zero_local)),
                                                tcx.types.u64,
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_nonzero_u64_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Sub,
                                                Box::new((
                                                    self.const_u64(tcx, source_info.span, 1),
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(keep_tag_part_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Mul,
                                                Box::new((
                                                    Operand::Copy(Place::from(
                                                        tag_is_nonzero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(tl)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(anchor_part_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Mul,
                                                Box::new((
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(anchor_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(selected_tag_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Add,
                                                Box::new((
                                                    Operand::Copy(Place::from(keep_tag_part_local)),
                                                    Operand::Copy(Place::from(anchor_part_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                ]);
                                Operand::Copy(Place::from(selected_tag_local))
                            } else {
                                Operand::Copy(Place::from(anchor_local))
                            }
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                            Operand::Copy(Place::from(tl))
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
                        if matches!(place.projection.first(), Some(ProjectionElem::Deref))
                            && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                        {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                Operand::Copy(Place::from(tl))
                            } else if let Some(tl) =
                                ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                            {
                                Operand::Copy(Place::from(tl))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    place,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    true,
                                    ParentSelectionMode::PointeeFamily,
                                )
                            }
                        } else {
                            self.parent_tag_operand_for_src_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                source_info,
                                place,
                                tag_local_for_ptr_local,
                                ref_ancestor_local_for_ptr_local,
                                reborrow_anchor_local_for_stack_local,
                                projectionless_anchor_suppressed_locals,
                                false,
                                true,
                                ParentSelectionMode::PointeeFamily,
                            )
                        }
                    };
                    let tag_op: Operand<'tcx> = if (flags & CALL_ARG_FLAG_USE_EXPORT_PARENT) != 0 {
                        if let (Some(export_parent_local), Some(recovered_local)) = (
                            export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                            export_parent_is_recovered_local_for_ptr_local
                                .get(&ptr_local)
                                .copied(),
                        ) {
                            let fallback_tag_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));
                            let recovered_u64_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));
                            let not_recovered_u64_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));
                            let fallback_part_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));
                            let export_part_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));
                            let selected_tag_local = body
                                .local_decls
                                .push(LocalDecl::new(tcx.types.u64, source_info.span));

                            extra_stmts.extend([
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(fallback_tag_local),
                                        Rvalue::Use(fallback_tag_op.clone()),
                                    ))),
                                ),
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(recovered_u64_local),
                                        Rvalue::Cast(
                                            CastKind::IntToInt,
                                            Operand::Copy(Place::from(recovered_local)),
                                            tcx.types.u64,
                                        ),
                                    ))),
                                ),
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(not_recovered_u64_local),
                                        Rvalue::BinaryOp(
                                            BinOp::Sub,
                                            Box::new((
                                                self.const_u64(tcx, source_info.span, 1),
                                                Operand::Copy(Place::from(recovered_u64_local)),
                                            )),
                                        ),
                                    ))),
                                ),
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(fallback_part_local),
                                        Rvalue::BinaryOp(
                                            BinOp::Mul,
                                            Box::new((
                                                Operand::Copy(Place::from(not_recovered_u64_local)),
                                                Operand::Copy(Place::from(fallback_tag_local)),
                                            )),
                                        ),
                                    ))),
                                ),
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(export_part_local),
                                        Rvalue::BinaryOp(
                                            BinOp::Mul,
                                            Box::new((
                                                Operand::Copy(Place::from(recovered_u64_local)),
                                                Operand::Copy(Place::from(export_parent_local)),
                                            )),
                                        ),
                                    ))),
                                ),
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(selected_tag_local),
                                        Rvalue::BinaryOp(
                                            BinOp::Add,
                                            Box::new((
                                                Operand::Copy(Place::from(fallback_part_local)),
                                                Operand::Copy(Place::from(export_part_local)),
                                            )),
                                        ),
                                    ))),
                                ),
                            ]);

                            Operand::Copy(Place::from(selected_tag_local))
                        } else {
                            fallback_tag_op
                        }
                    } else {
                        fallback_tag_op
                    };

                    let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                    let arg_index = self.const_u64(tcx, source_info.span, arg_index);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_callee,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_index,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, flags),
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgValidate { local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op = self.parent_tag_operand_for_src_place(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        place,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        false,
                        true,
                        ParentSelectionMode::PointeeFamily,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();

                    let _ = local;
                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrUse { ptr_local } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
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
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            ParentSelectionMode::PointeeFamily,
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::DebugRefActivate {
                    raw_local,
                    tag_local,
                    is_mut,
                } => {
                    let raw_tag_op = if let Some(tl) = tag_local_for_ptr_local.get(&raw_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                    let ref_ancestor_op =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&raw_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let alias_exempt =
                        self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[raw_local].ty);
                    let arg_alias =
                        self.const_u8(tcx, source_info.span, if alias_exempt { 1 } else { 0 });
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        raw_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, raw_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: raw_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tag_local))
                }

                InstrKind::ShadowStore { src_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let ref_ancestor_op: Operand<'tcx> =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let export_parent_op: Operand<'tcx> = if let Some(tl) =
                        export_parent_local_for_ptr_local.get(&src_local)
                    {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        tag_op.clone()
                    };
                    let recovered_op: Operand<'tcx> = if let Some(tl) =
                        export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                    {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u8(tcx, source_info.span, 0)
                    };
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: export_parent_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: recovered_op,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::ShadowKill { ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagKill { ptr_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op = tag_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                        .map(|tag_local| Operand::Copy(Place::from(tag_local)))
                        .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagLocalKill { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagRetain { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrDerive {
                    dst,
                    src,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDerive dst");

                    let parent_from_src: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src) {
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
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = self.align_operand_for_ptr_derive(
                        tcx,
                        body,
                        src,
                        dst,
                        is_ref,
                        source_info.span,
                    );
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                InstrKind::PtrDeriveParent {
                    dst,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDeriveParent dst");
                    let parent_local = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for PtrDeriveParent dst");

                    let parent_tag_op: Operand<'tcx> = Operand::Copy(Place::from(parent_local));

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                _ => {
                    let is_mut_u8: u8 = match creation_kind {
                        InstrKind::Ref {
                            bk: borrow_kind, ..
                        } => match borrow_kind {
                            BorrowKind::Mut { .. } => 1,
                            _ => 0,
                        },
                        InstrKind::Raw { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        InstrKind::RetRoot { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        _ => 0,
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, is_mut_u8);

                    let arg_parent: Operand<'tcx> = match &creation_kind {
                        InstrKind::Ref { bk, src, .. } => {
                            let use_projectionless_anchor = matches!(bk, BorrowKind::Mut { .. })
                                || self.compile_alias_model_is_sb_like();
                            if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
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
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        use_projectionless_anchor,
                                    ),
                                )
                            }
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
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    true,
                                    true,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        true,
                                    ),
                                )
                            }
                        }
                        _ => self.const_u64(tcx, source_info.span, 0),
                    };

                    let alias_exempt = match &creation_kind {
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            if let Some(dst_local) = place.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                self.alias_exempt_for_ptr_ty(tcx, body, dst_ty)
                                    || self.alias_exempt_child_from_source_ptr(
                                        tcx,
                                        body,
                                        src.ty(&body.local_decls, tcx).ty,
                                        dst_ty,
                                    )
                            } else {
                                let ty = src.ty(&body.local_decls, tcx).ty;
                                self.alias_exempt_for_ty(tcx, body, ty)
                            }
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
                    // - bit6: strict creation-time provenance check for projected/derived raws
                    // - bit7: deref-based raw creation must reject exposed/no-provenance parents
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
                            if self.is_pointer_ty(src_ty) && !self.is_thin_ptr_ty(tcx, body, src_ty)
                            {
                                // Wide-pointer reborrows often lower through a temporary thin raw
                                // data pointer. If that helper raw root loses lineage, the runtime
                                // should prefer dropping the bad raw-root parent over freezing the
                                // eventual wide ref/write as a foreign sibling.
                                flags |= 0b1_0000;
                            }
                            if src.projection.is_empty()
                                && projectionless_anchor_suppressed_locals.contains(&src.local)
                            {
                                // Whole-slot reborrows of by-value return carriers (`other:
                                // BytesMut; &mut other`) need targeted same-slot root retirement
                                // in TB-lite. Mark only this path so ordinary repeated `&mut`
                                // call arguments do not invalidate each other.
                                flags |= 0b1000;
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
                                flags |= 0b0100_0000;
                            }
                            if matches!(src.projection.first(), Some(ProjectionElem::Deref))
                                && !self
                                    .raw_creation_allows_no_provenance_transport(tcx, body, *src)
                            {
                                flags |= 0b1000_0000;
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
                            if alias_exempt {
                                1
                            } else {
                                0
                            }
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
                            InstrKind::Ref { .. } => self
                                .ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                ),
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
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = match &creation_kind {
                        InstrKind::Ref { src, .. } => self
                            .align_operand_for_ref_creation_src_place(
                                tcx,
                                body,
                                place.ty(&body.local_decls, tcx).ty,
                                *src,
                                source_info.span,
                            ),
                        InstrKind::Raw { src, .. } => {
                            self.align_operand_for_src_place(tcx, body, *src, source_info.span)
                        }
                        _ => bounds_ptr_local
                            .map(|pl| {
                                self.align_operand_for_ptr_local(tcx, body, pl, source_info.span)
                            })
                            .unwrap_or_else(|| {
                                SizeOperand::Const(self.const_usize(tcx, source_info.span, 0))
                            }),
                    };
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_parent,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);

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
                InstrKind::Ref { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let use_projectionless_anchor = match &creation_kind {
                                InstrKind::Ref { bk, .. } => {
                                    matches!(bk, BorrowKind::Mut { .. })
                                        || self.compile_alias_model_is_sb_like()
                                }
                                _ => true,
                            };
                            let parent_op = if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
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
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        use_projectionless_anchor,
                                    ),
                                )
                            };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(parent_op),
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
                InstrKind::RawRoot { ptr_local, .. } => ref_ancestor_local_for_ptr_local
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
                    }),
                InstrKind::RetRoot {
                    dst_local, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst_local).copied()
                    {
                        if *is_ref {
                            tag_local_for_ptr_local
                                .get(dst_local)
                                .copied()
                                .map(|dst_tag_local| {
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
                            let src_parent_op: Operand<'tcx> =
                                if let Some(src_tag_local) = tag_local_for_ptr_local.get(src) {
                                    Operand::Copy(Place::from(*src_tag_local))
                                } else if let Some(src_ref_ancestor_local) =
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
                                    Rvalue::Use(src_parent_op),
                                ))),
                            ))
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
            let export_parent_init_stmts: Vec<Statement<'tcx>> = {
                let direct_dst_local = match &creation_kind {
                    InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                    InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                    InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                    InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => {
                        Some(*dst)
                    }
                    InstrKind::DebugRefActivate { raw_local, .. } => Some(*raw_local),
                    _ => None,
                };
                if let Some(dst_local) = direct_dst_local {
                    let mut stmts = Vec::new();
                    let mut recovered_init_local: Option<Local> = None;
                    if let (Some(export_parent_local), Some(dst_tag_local)) = (
                        export_parent_local_for_ptr_local.get(&dst_local).copied(),
                        tag_local_for_ptr_local.get(&dst_local).copied(),
                    ) {
                        let default_export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref {
                                bk,
                                src,
                                projected_reborrow_anchor_key,
                                ..
                            } if matches!(bk, BorrowKind::Mut { .. })
                                && matches!(
                                    body.local_decls[dst_local].ty.kind(),
                                    TyKind::Ref(_, pointee_ty, Mutability::Mut)
                                        if !self.is_pointer_ty(*pointee_ty)
                                ) =>
                            {
                                projected_reborrow_anchor_key
                                    .as_ref()
                                    .and_then(|key| {
                                        projected_reborrow_anchor_local_for_key.get(key).copied()
                                    })
                                    .or_else(|| {
                                        if src.projection.is_empty() {
                                            reborrow_anchor_local_for_stack_local
                                                .get(&src.local)
                                                .copied()
                                        } else {
                                            None
                                        }
                                    })
                                    .map(|anchor_local| Operand::Copy(Place::from(anchor_local)))
                                    .unwrap_or_else(|| Operand::Copy(Place::from(dst_tag_local)))
                            }
                            _ => Operand::Copy(Place::from(dst_tag_local)),
                        };
                        let export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::Raw { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::PtrDerive {
                                src,
                                is_ref: true,
                                ..
                            } => {
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(src).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(src)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            _ => default_export_parent_op,
                        };
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_op),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&dst_local)
                        .copied()
                    {
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(
                                    recovered_init_local
                                        .map(|local| Operand::Copy(Place::from(local)))
                                        .unwrap_or_else(|| self.const_u8(tcx, source_info.span, 0)),
                                ),
                            ))),
                        ));
                    }
                    stmts
                } else {
                    Vec::new()
                }
            };
            let reborrow_anchor_set_stmts: Vec<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref {
                    bk,
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } => {
                    if matches!(bk, BorrowKind::Mut { .. }) {
                        if let Some(dst_local) = place.as_local() {
                            let anchor_local_opt = projected_reborrow_anchor_key
                                .as_ref()
                                .and_then(|key| projected_reborrow_anchor_local_for_key.get(key))
                                .copied()
                                .or_else(|| {
                                    if src.projection.is_empty() {
                                        reborrow_anchor_local_for_stack_local
                                            .get(&src.local)
                                            .copied()
                                    } else {
                                        None
                                    }
                                });
                            let anchor_state_local_opt = if src.projection.is_empty() {
                                anchor_is_slot_family_local_for_stack_local
                                    .get(&src.local)
                                    .copied()
                            } else {
                                None
                            };
                            if let Some(anchor_local) = anchor_local_opt {
                                if let Some(anchor_source_local) = projected_ref_parent_local
                                    .or_else(|| tag_local_for_ptr_local.get(&dst_local).copied())
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    let mut stmts = vec![
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_is_zero_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Eq,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        self.const_u64(tcx, source_info.span, 0),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ];
                                    if let Some(anchor_state_local) = anchor_state_local_opt {
                                        stmts.push(Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_state_local),
                                                Rvalue::Use(self.const_u8(
                                                    tcx,
                                                    source_info.span,
                                                    1,
                                                )),
                                            ))),
                                        ));
                                    }
                                    stmts
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if src.projection.is_empty() {
                            if let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                                .get(&src.local)
                                .copied()
                            {
                                if let Some(anchor_source_local) =
                                    tag_local_for_ptr_local.get(&dst_local).copied()
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    vec![
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_is_zero_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Eq,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        self.const_u64(tcx, source_info.span, 0),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ]
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };

            let split_at = {
                let len = body.basic_blocks[bb].statements.len();
                if stmt_idx >= len {
                    len
                } else if insert_before {
                    stmt_idx
                } else {
                    stmt_idx + 1
                }
            };
            let split_at_prefix = split_at.min(orig_stmt_len);
            orig_stmt_prefix_len.insert(bb, split_at_prefix);
            orig_stmt_prefix_len.insert(cont_block, orig_stmt_len.saturating_sub(split_at_prefix));

            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

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

            if !reborrow_anchor_set_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, reborrow_anchor_set_stmts);
            }
            if !export_parent_init_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, export_parent_init_stmts);
            }
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
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
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
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
                    Spanned {
                        node: arg_align,
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
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
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
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(
                                    tcx,
                                    source_info.span,
                                    recovered_value,
                                )),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let local_ty = body.local_decls[local].ty;
                if self.is_box_ty(tcx, local_ty) {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (take_addr_stmt1, take_addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let pointee_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let Some((pointee_addr_stmt1, pointee_addr_stmt2)) = self
                        .box_pointee_slot_addr_stmts_for_local(
                            tcx,
                            body,
                            source_info,
                            local,
                            pointee_addr_local,
                        )
                    else {
                        panic!("ArgAnchorTake box local missing pointee address support");
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
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(pointee_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 1),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
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
                        bd.statements.push(take_addr_stmt1);
                        bd.statements.push(take_addr_stmt2);
                        bd.statements.push(pointee_addr_stmt1);
                        bd.statements.push(pointee_addr_stmt2);
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                } else {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (addr_stmt1, addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let direct_ref_field = self.first_direct_ref_field_place(tcx, body, local);
                    let record_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (record_addr_stmt1_opt, record_addr_stmt2, record_is_mut, record_ty) =
                        if let Some((field_place, pointee_ty, is_mut)) = direct_ref_field {
                            let (field_addr_stmt1_opt, field_addr_stmt2) = self
                                .addr_stmts_for_place(
                                    tcx,
                                    body,
                                    source_info,
                                    field_place,
                                    record_addr_local,
                                )
                                .expect("ArgAnchorTake direct ref field address");
                            (field_addr_stmt1_opt, field_addr_stmt2, is_mut, pointee_ty)
                        } else {
                            (
                                None,
                                Statement::new(
                                    source_info,
                                    StatementKind::Assign(Box::new((
                                        Place::from(record_addr_local),
                                        Rvalue::Use(Operand::Copy(Place::from(take_addr_local))),
                                    ))),
                                ),
                                true,
                                local_ty,
                            )
                        };
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let size_op = self.size_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    let align_op =
                        self.align_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (align_len, mut align_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);

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
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(record_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(
                                        tcx,
                                        source_info.span,
                                        if record_is_mut { 1 } else { 0 },
                                    ),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: bounds_len,
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: align_len,
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
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
                        bd.statements.push(addr_stmt1);
                        bd.statements.push(addr_stmt2);
                        if let Some(record_addr_stmt1) = record_addr_stmt1_opt {
                            bd.statements.push(record_addr_stmt1);
                        }
                        bd.statements.push(record_addr_stmt2);
                        if !bounds_len_stmts.is_empty() {
                            bd.statements.append(&mut bounds_len_stmts);
                        }
                        if !align_len_stmts.is_empty() {
                            bd.statements.append(&mut align_len_stmts);
                        }
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                }
            }
        }
    }

    fn insert_fallback_return_stack_allocs<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        fallback_return_locals: &[(Local, SizeOperand<'tcx>)],
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        if fallback_return_locals.is_empty() {
            return;
        }

        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            if !matches!(term.kind, TerminatorKind::Return) {
                continue;
            }

            for (local, size_op) in fallback_return_locals.iter().cloned() {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(local),
                    kind: InstrKind::StackAlloc {
                        local,
                        live: false,
                        size_op: size_op.clone(),
                    },
                });
                self.trace_stack_alloc_emit(tcx, body, local, false, &size_op, "FallbackReturn");
            }
        }

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            manual_holder_managed_tag_assignments,
            tag_local_for_ptr_local,
            local_slot_shadow_store_locals,
            export_parent_local_for_ptr_local,
            export_parent_is_recovered_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
            reborrow_anchor_local_for_stack_local,
            anchor_is_slot_family_local_for_stack_local,
            projectionless_anchor_suppressed_locals,
            projected_reborrow_anchor_local_for_key,
            debug_ref_bindings,
            hooks,
        );
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
                                println!(
                                    " - AsyncDropGlue for DefId: {:?}, type: {:?}",
                                    def_id, ty
                                );
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
                    ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                        *def_id
                    }
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
            .find_runtime_fn_def_id(tcx, "__record_ref_creation", 6)
            .expect("missing '__record_ref_creation' definition");
        let def_id_debug_ref = self
            .find_runtime_fn_def_id(tcx, "__record_debug_ref_creation", 7)
            .expect("missing '__record_debug_ref_creation' definition");
        let def_id_raw = self
            .find_runtime_fn_def_id(tcx, "__record_raw_ptr_creation", 6)
            .expect("missing '__record_raw_ptr_creation' definition");
        let def_id_alloc = self
            .find_runtime_fn_def_id(tcx, "__rz_record_alloc", 3)
            .expect("missing '__rz_record_alloc' definition");
        let def_id_write = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write", 5)
            .expect("missing '__rz_ptr_write' definition");
        let def_id_write_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write_allow_untagged", 5)
            .expect("missing '__rz_ptr_write_allow_untagged' definition");
        let def_id_local_write_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_local_write_allow_untagged", 3)
            .expect("missing '__rz_local_write_allow_untagged' definition");
        let def_id_read = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read", 5)
            .expect("missing '__rz_ptr_read' definition");
        let def_id_read_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read_allow_untagged", 5)
            .expect("missing '__rz_ptr_read_allow_untagged' definition");
        let def_id_use = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_use", 2)
            .expect("missing '__rz_ptr_use' definition");
        let def_id_push_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_call_arg_tag", 5)
            .expect("missing '__rz_push_call_arg_tag' definition");
        let def_id_validate_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_call_arg_tag", 1)
            .expect("missing '__rz_validate_call_arg_tag' definition");
        let def_id_take_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_tag", 4)
            .expect("missing '__rz_take_call_arg_tag' definition");
        let def_id_take_call_arg_tag_anchor = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_tag_anchor", 3)
            .expect("missing '__rz_take_call_arg_tag_anchor' definition");
        let def_id_push_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_ret_tag", 3)
            .expect("missing '__rz_push_ret_tag' definition");
        let def_id_validate_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_ret_tag", 2)
            .expect("missing '__rz_validate_ret_tag' definition");
        let def_id_take_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_tag", 2)
            .expect("missing '__rz_take_ret_tag' definition");
        let def_id_push_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_ret_leaf_shadow", 3)
            .expect("missing '__rz_push_ret_leaf_shadow' definition");
        let def_id_take_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_leaf_shadow", 3)
            .expect("missing '__rz_take_ret_leaf_shadow' definition");
        let def_id_validate_loaded_ref_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_loaded_ref_tag", 1)
            .expect("missing '__rz_validate_loaded_ref_tag' definition");
        let def_id_require_loaded_ptr_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_require_loaded_ptr_tag", 1)
            .expect("missing '__rz_require_loaded_ptr_tag' definition");
        let def_id_take_ret_tag_or_root = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_tag_or_root", 6)
            .expect("missing '__rz_take_ret_tag_or_root' definition");
        let def_id_push_mut_arg_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_mut_arg_ret_tag", 4)
            .expect("missing '__rz_push_mut_arg_ret_tag' definition");
        let def_id_take_mut_arg_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_tag", 3)
            .expect("missing '__rz_take_mut_arg_ret_tag' definition");
        let def_id_take_mut_arg_ret_tag_or_zero = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_tag_or_zero", 3)
            .expect("missing '__rz_take_mut_arg_ret_tag_or_zero' definition");
        let def_id_push_mut_arg_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_mut_arg_ret_leaf_shadow", 5)
            .expect("missing '__rz_push_mut_arg_ret_leaf_shadow' definition");
        let def_id_take_mut_arg_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_leaf_shadow", 5)
            .expect("missing '__rz_take_mut_arg_ret_leaf_shadow' definition");
        let def_id_exit_fn = self
            .find_runtime_fn_def_id(tcx, "__rz_exit_fn", 1)
            .expect("missing '__rz_exit_fn' definition");
        let def_id_shadow_store_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_store_ptr", 5)
            .expect("missing '__rz_shadow_store_ptr' definition");
        let def_id_shadow_store_ptr_local = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_store_ptr_local", 5)
            .expect("missing '__rz_shadow_store_ptr_local' definition");
        let def_id_shadow_load_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_tag", 1)
            .expect("missing '__rz_shadow_load_tag' definition");
        let def_id_shadow_load_ref_ancestor = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_ref_ancestor", 1)
            .expect("missing '__rz_shadow_load_ref_ancestor' definition");
        let def_id_shadow_load_export_parent = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent", 1)
            .expect("missing '__rz_shadow_load_export_parent' definition");
        let def_id_shadow_load_export_parent_recovered = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent_recovered", 1)
            .expect("missing '__rz_shadow_load_export_parent_recovered' definition");
        let def_id_shadow_kill_range = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_kill_range", 2)
            .expect("missing '__rz_shadow_kill_range' definition");
        let def_id_tag_kill = self
            .find_runtime_fn_def_id(tcx, "__rz_tag_kill", 1)
            .expect("missing '__rz_tag_kill' definition");
        let def_id_tag_retain = self
            .find_runtime_fn_def_id(tcx, "__rz_tag_retain", 1)
            .expect("missing '__rz_tag_retain' definition");
        let def_id_shadow_copy_slot = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_copy_slot", 2)
            .expect("missing '__rz_shadow_copy_slot' definition");
        let def_id_shadow_copy_range = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_copy_range", 3)
            .expect("missing '__rz_shadow_copy_range' definition");

        let hooks = Hooks {
            def_id_ref,
            def_id_debug_ref,
            def_id_raw,
            def_id_alloc,
            def_id_write,
            def_id_write_allow_untagged,
            def_id_local_write_allow_untagged,
            def_id_read,
            def_id_read_allow_untagged,
            def_id_use,
            def_id_push_call_arg_tag,
            def_id_validate_call_arg_tag,
            def_id_take_call_arg_tag,
            def_id_take_call_arg_tag_anchor,
            def_id_push_ret_tag,
            def_id_validate_ret_tag,
            def_id_take_ret_tag,
            def_id_push_ret_leaf_shadow,
            def_id_take_ret_leaf_shadow,
            def_id_validate_loaded_ref_tag,
            def_id_require_loaded_ptr_tag,
            def_id_take_ret_tag_or_root,
            def_id_push_mut_arg_ret_tag,
            def_id_take_mut_arg_ret_tag,
            def_id_take_mut_arg_ret_tag_or_zero,
            def_id_push_mut_arg_ret_leaf_shadow,
            def_id_take_mut_arg_ret_leaf_shadow,
            def_id_exit_fn,
            def_id_shadow_store_ptr,
            def_id_shadow_store_ptr_local,
            def_id_shadow_load_tag,
            def_id_shadow_load_ref_ancestor,
            def_id_shadow_load_export_parent,
            def_id_shadow_load_export_parent_recovered,
            def_id_shadow_kill_range,
            def_id_tag_kill,
            def_id_tag_retain,
            def_id_shadow_copy_slot,
            def_id_shadow_copy_range,
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
        let reborrow_anchor_stack_locals = scan.interesting_stack_locals.clone();
        let tag_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        let export_parent_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        let export_parent_is_recovered_local_for_ptr_local =
            self.allocate_u8_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
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
        let reborrow_anchor_stack_local_keys: HashSet<Local> = reborrow_anchor_stack_locals
            .iter()
            .copied()
            .filter(|local| !self.is_pointer_ty(body.local_decls[*local].ty))
            .collect();
        let reborrow_anchor_local_for_stack_local =
            self.allocate_tag_locals(tcx, body, reborrow_anchor_stack_local_keys.clone());
        let anchor_is_slot_family_local_for_stack_local =
            self.allocate_u8_locals(tcx, body, reborrow_anchor_stack_local_keys);
        let projected_reborrow_anchor_local_for_key =
            self.allocate_reborrow_anchor_locals(tcx, body, &scan.projected_reborrow_anchor_specs);

        let mut debug_ref_bindings: HashMap<DebugRefBindingKey, DebugRefBinding> = HashMap::new();
        for (key, is_mut) in self.collect_debug_ref_bindings(tcx, body) {
            let tag_local = body.local_decls.push(LocalDecl::new(
                tcx.types.u64,
                body.source_scopes[key.scope].span,
            ));
            debug_ref_bindings.insert(
                key,
                DebugRefBinding {
                    key,
                    tag_local,
                    is_mut,
                },
            );
        }
        let mut insert_points = scan.insert_points;
        if !scan.return_sites.is_empty() {
            let mut exit_kill_tag_locals: Vec<Local> =
                tag_local_for_ptr_local.values().copied().collect();
            exit_kill_tag_locals.sort_by_key(|local| local.index());
            exit_kill_tag_locals.dedup();
            for (bb, source_info, stmt_idx) in &scan.return_sites {
                for tag_local in &exit_kill_tag_locals {
                    insert_points.push(InsertPoint {
                        bb: *bb,
                        stmt_idx: *stmt_idx,
                        insert_before: false,
                        source_info: *source_info,
                        place: Place::from(*tag_local),
                        kind: InstrKind::TagLocalKill {
                            tag_local: *tag_local,
                        },
                    });
                }
            }
        }
        for binding in debug_ref_bindings.values() {
            let activation_locs =
                self.debug_ref_activation_locations(body, binding.key.scope, binding.key.raw_local);
            for (bb, stmt_idx, source_info) in activation_locs {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info,
                    place: Place::from(binding.key.raw_local),
                    kind: InstrKind::DebugRefActivate {
                        raw_local: binding.key.raw_local,
                        tag_local: binding.tag_local,
                        is_mut: binding.is_mut,
                    },
                });
            }
        }
        // Box::from_raw rewraps an existing allocation. Suppress any dead HeapAlloc
        // hook placed at its call site so drop can read the pointee safely.
        insert_points.retain(|ip| {
            if let InstrKind::HeapAlloc {
                ptr_local,
                live: false,
                ..
            } = ip.kind
            {
                if let Some(term) = body.basic_blocks[ip.bb].terminator.as_ref() {
                    if let TerminatorKind::Call {
                        args, destination, ..
                    } = &term.kind
                    {
                        let arg0_local = args
                            .get(0)
                            .and_then(|arg| self.place_from_operand(&arg.node))
                            .map(|p| p.local);
                        if arg0_local == Some(ptr_local) {
                            if let Some(dst_local) = destination.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                if let TyKind::Adt(adt, _) = dst_ty.kind() {
                                    let name = tcx.def_path_str(adt.did());
                                    if name.contains("boxed::Box") || name.contains("::boxed::Box")
                                    {
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
        self.schedule_reborrow_anchor_resets(
            body,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projected_reborrow_anchor_specs,
            &projected_reborrow_anchor_local_for_key,
            &mut insert_points,
        );
        self.schedule_reborrow_anchor_propagation(
            tcx,
            body,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &mut insert_points,
        );

        let mut manual_holder_managed_tag_assignments: HashSet<(BasicBlock, usize)> =
            HashSet::new();

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
            hooks,
        );
        self.insert_fallback_return_stack_allocs(
            tcx,
            body,
            &scan.fallback_return_locals,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
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
            &export_parent_local_for_ptr_local,
            &arg_ptr_locals,
        );
        self.init_tag_locals_to_zero(
            tcx,
            body,
            &ref_ancestor_local_for_ptr_local,
            &arg_ptr_locals,
        );
        self.init_extra_u8_locals_to_zero(
            tcx,
            body,
            &export_parent_is_recovered_local_for_ptr_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &reborrow_anchor_local_for_stack_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_u8_locals_to_zero(
            tcx,
            body,
            &anchor_is_slot_family_local_for_stack_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &projected_reborrow_anchor_local_for_key
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &debug_ref_bindings
                .values()
                .map(|binding| binding.tag_local)
                .collect::<Vec<_>>(),
        );

        let mut holder_points: Vec<InsertPoint<'tcx>> = Vec::new();
        self.schedule_tag_local_holder_updates(
            body,
            &tag_local_for_ptr_local,
            &manual_holder_managed_tag_assignments,
            &mut holder_points,
        );
        self.insert_instrumentation(
            tcx,
            body,
            holder_points,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
            hooks,
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
        CallEffect::LoadUnaligned => "LoadUnaligned",
        CallEffect::StoreUnaligned => "StoreUnaligned",
        CallEffect::PtrDerive => "PtrDerive",
        CallEffect::ExposedProvenanceRoot => "ExposedProvenanceRoot",
        CallEffect::CarrierCopyArg0 => "CarrierCopyArg0",
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
            CallEffect::CarrierCopyArg0
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
            effect_for("core::slice::<impl [T]>::split_at"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::slice::<impl [T]>::split_at_mut"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(
            effect_for("core::str::<impl str>::as_bytes"),
            CallEffect::PtrDerive
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
            effect_for(
                "core::slice::index::<impl std::slice::SliceIndex<[u8]> for std::ops::Range<usize>>::index_mut"
            ),
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
            effect_for("core::sync::atomic::AtomicPtr::<T>::new"),
            CallEffect::CarrierCopyArg0
        );
        assert_eq!(effect_for("core::ptr::null"), CallEffect::Ignore);
        assert_eq!(
            effect_for("std::io::Cursor::<T>::get_ref"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("std::io::Cursor::<T>::position"),
            CallEffect::Ignore
        );
        assert_eq!(
            effect_for("alloc::collections::VecDeque::<T>::as_slices"),
            CallEffect::CarrierCopyArg0
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
            MyOptimizationPass.classify_call_effect("std::hint::black_box"),
            CallEffect::PtrDerive
        );
        assert_eq!(
            effect_for("<alloc::vec::Vec<T, A> as core::ops::Index<I>>::index"),
            CallEffect::PtrDerive
        );
    }
}
