use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::{Mutex, OnceLock};

// (rest unchanged)
// NOTE: This pass intentionally avoids instrumenting std/core/alloc directly.
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::Mutability;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::{GlobalAlloc, Scalar};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum PassLogLevel {
    Warn,
    Info,
    Trace,
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

    // Slice pointer extraction wrappers.
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::as_ptr", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::slice::<impl [", MatchKind::EndsWith, "::as_mut_ptr", CallEffect::PtrDerive),

    // Vec pointer extraction wrappers. Covers `alloc::vec::Vec` and `std::vec::Vec`, including monomorphized forms.
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::as_ptr", CallEffect::PtrDerive),
    EffectRule::two(MatchKind::Contains, "::vec::Vec", MatchKind::EndsWith, "::as_mut_ptr", CallEffect::PtrDerive),

    // Volatile wrappers (free functions).
    EffectRule::one(MatchKind::Contains, "::ptr::read_volatile", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::ptr::write_volatile", CallEffect::Store),
    // Volatile intrinsics.
    EffectRule::one(MatchKind::Contains, "::intrinsics::volatile_load", CallEffect::Load),
    EffectRule::one(MatchKind::Contains, "::intrinsics::volatile_store", CallEffect::Store),

    // Memset-like.
    EffectRule::one(MatchKind::Contains, "::intrinsics::write_bytes", CallEffect::MemSet),
    // Method-style wrappers (e.g. std::ptr::mut_ptr::<impl *mut T>::write_bytes)
    EffectRule::one(MatchKind::EndsWith, "::write_bytes", CallEffect::MemSet),

    // Plain wrappers.
    // Use suffix matching for `read`/`write` so we don't accidentally match `write_bytes`/`read_bytes`.
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
    /// A write through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrWrite { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// A read through a pointer local.
    /// `size_op` is best-effort (0 = unknown). Kept as an operand so we can pass dynamic sizes.
    PtrRead { ptr_local: Local, size_op: SizeOperand<'tcx> },
    /// Coarse pointer-use tracking: a pointer-typed local appears in a call argument.
    /// This is treated as an escape event at call boundaries.
    PtrUse { ptr_local: Local },
    /// Propagate tags across pointer-to-pointer casts and plain copies/moves of pointer locals.
    /// This is a local tag assignment, not a runtime hook.
    TagProp { dst: Local, src: Local },
    /// Fresh tag for a derived pointer value (pointer arithmetic like add/sub/offset).
    /// Emits a runtime raw-pointer creation hook with `parent=tag(src)` and assigns into `tag(dst)`.
    PtrDerive { dst: Local, src: Local, is_mut: bool },
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
    def_id_read: DefId,
    def_id_use: DefId,
    def_id_push_call_arg_tag: DefId,
    def_id_take_call_arg_tag: DefId,
    def_id_push_ret_tag: DefId,
    def_id_take_ret_tag: DefId,
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
    fn is_thin_ptr_ty<'tcx>(&self, _tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => match pointee.kind() {
                TyKind::Slice(..) | TyKind::Str | TyKind::Dynamic(..) => false,
                // `extern type` is unsized but uses `()` metadata, so pointers are thin.
                TyKind::Foreign(..) => true,
                _ => true,
            },
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
    /// if it hasn't been tagged yet. Only applies to THIN pointers.
    fn ensure_raw_root_before<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        ptr_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        if tagged_ptr_locals.contains(&ptr_local) {
            return;
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_thin_ptr_ty(tcx, ptr_ty) {
            // Do not attempt to RawRoot-tag wide pointers.
            return;
        }

        let is_mut = self.ptr_is_mut(ptr_ty);
        tagged_ptr_locals.insert(ptr_local);
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before: true,
            source_info,
            place: Place::from(ptr_local),
            kind: InstrKind::RawRoot { ptr_local, is_mut },
        });
    }

    /// Build statements that compute `addr_local` from a pointer-typed place.
    /// For thin pointers we can expose provenance directly.
    /// For wide pointers we first extract the data pointer, then expose provenance on that
    /// thin pointer so codegen does not ICE and the runtime observes the data address.
    fn addr_stmts_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
    ) -> Option<(Option<Statement<'tcx>>, Statement<'tcx>)> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_thin_ptr_ty(tcx, place_ty) {
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

        if self.is_pointer_ty(place_ty) {
            let data_ptr_ty = self.data_ptr_ty_for_ptr(tcx, place_ty)?;
            let data_ptr_local = body
                .local_decls
                .push(LocalDecl::new(data_ptr_ty, source_info.span));

            // `PtrToPtr` extracts the data pointer for wide pointers like slices and str,
            // keeping only the address portion of the scalar pair.
            let data_ptr_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(data_ptr_local),
                    Rvalue::Cast(
                        CastKind::PtrToPtr,
                        Operand::Copy(place),
                        data_ptr_ty,
                    ),
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

            return Some((Some(data_ptr_stmt), addr_stmt));
        }

        None
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

    /// Print an "unknown call" warning once per callee def-path to avoid spam.
    fn warn_unknown_call_once(&self, def_path: &str) {
        static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut guard = set.lock().unwrap();
        if guard.insert(def_path.to_string()) {
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
        matches!(
            name,
            "core"
                | "alloc"
                | "std"
                | "proc_macro"
                | "test"
                | "panic_abort"
                | "panic_unwind"
                | "compiler_builtins"
                | "unwind"
                | "cfg_if"
        )
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
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if !instrument_all_deps {
                return HashSet::new();
            }

            let mut set = HashSet::new();
            for &cnum in tcx.crates(()).iter() {
                let name = tcx.crate_name(cnum).as_str().to_string();
                if name == "runtime" {
                    continue;
                }

                // Even in "instrument all deps" mode, do NOT treat std/core/alloc and other
                // std-like crates as instrumented callees. We rely on boundary interception
                // (global allocator wrapper) and wrapper classification instead.
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

        // Always treat the local crate as instrumented.
        if def_id.krate == LOCAL_CRATE {
            return true;
        }

        // Optional allowlist (RZ_INSTRUMENTED_CRATES) or "instrument all deps" mode (RZ_INSTRUMENT_ALL_DEPS=1).
        let allow = self.instrumented_crates_cached(tcx);
        allow.contains(crate_name)
    }

    fn place_from_operand<'tcx>(&self, op: &Operand<'tcx>) -> Option<Place<'tcx>> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => Some(*p),
            _ => None,
        }
    }

    fn match_call_effect_rule(&self, def_path: &str) -> Option<CallEffect> {
        // Strip only a *trailing* monomorphization like `::<T>`.
        // Do NOT strip generic args that appear in the middle of a path like
        // `std::vec::Vec::<T, A>::as_mut_ptr`, otherwise we lose the method suffix.
        let def_path_no_trailing_mono = {
            let s = def_path;
            if !s.ends_with('>') {
                s
            } else if let Some(pos) = s.rfind("::<") {
                // Only treat it as a trailing monomorphization if there is no further module separator
                // after the `::<`.
                if s[pos..].contains("::") {
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
                MatchKind::Contains => def_path.contains(r.needle1),
                MatchKind::EndsWith => def_path.ends_with(r.needle1) || def_path_no_trailing_mono.ends_with(r.needle1),
            };
            if !m1 {
                continue;
            }

            if let (Some(k2), Some(n2)) = (r.kind2, r.needle2) {
                let m2 = match k2 {
                    MatchKind::Contains => def_path.contains(n2),
                    MatchKind::EndsWith => def_path.ends_with(n2) || def_path_no_trailing_mono.ends_with(n2),
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
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }
        // Emit MIR size_of to avoid layout normalization during instrumentation.
        SizeOperand::SizeOf(ty)
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
            || const_ty.has_free_regions()
            || self.type_needs_normalization(const_ty)
            || !const_ty.is_global()
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
        if const_ty.has_param()
            || const_ty.has_infer()
            || const_ty.has_aliases()
            || const_ty.has_opaque_types()
            || const_ty.has_placeholders()
            || const_ty.has_bound_vars()
            || const_ty.has_free_regions()
            || self.type_needs_normalization(const_ty)
            || !const_ty.is_global()
        {
            return None;
        }

        if let TyKind::FnDef(def_id, args) = c.const_.ty().kind() {
            def_id_opt = Some(self.resolve_instance_def_id(tcx, body, *def_id, args));
        } else if let Some(scalar) =
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

    /// Compute the set of stack locals worth tracking as allocations.
    ///
    /// We track *pointee* locals whose address is taken (via `&` or `&raw`) so that range-based
    /// allocation lookup and OOB checks work for stack data.
    ///
    /// We intentionally do NOT treat pointer-typed locals or miscellaneous temporaries as
    /// allocations: aliasing models (e.g. Stacked Borrows) are enforced via pointer tags on
    /// READ/WRITE, not by recording the address of pointer locals as allocations.
    fn compute_interesting_stack_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> HashSet<Local> {
        let mut interesting: HashSet<Local> = HashSet::new();
        for (_bb, block_data) in body.basic_blocks.iter_enumerated() {
            for stmt in block_data.statements.iter() {
                if let StatementKind::Assign(box (_dst, rv)) = &stmt.kind {
                    match rv {
                        // Address-taken locals: these correspond to real stack slots that pointers can reference.
                        Rvalue::Ref(_, _bk, src_place) => {
                            if src_place.local != RETURN_PLACE {
                                let local_ty = body.local_decls[src_place.local].ty;
                                if !self.is_pointer_ty(local_ty) {
                                    interesting.insert(src_place.local);
                                }
                            }
                        }
                        Rvalue::RawPtr(_mutbl, src_place) => {
                            if src_place.local != RETURN_PLACE {
                                let local_ty = body.local_decls[src_place.local].ty;
                                if !self.is_pointer_ty(local_ty) {
                                    interesting.insert(src_place.local);
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
                        if local != RETURN_PLACE {
                            let local_ty = body.local_decls[local].ty;
                            if !self.is_pointer_ty(local_ty) {
                                interesting.insert(local);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        interesting
    }

    fn track_all_stack_allocs_flag(&self) -> bool {
        std::env::var("RZ_STACK_ALLOCS")
            .map(|v| v == "all" || v == "ALL" || v == "1" || v == "true" || v == "TRUE")
            .unwrap_or(false)
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

        // Retag all pointer arguments, including wide pointers, because later conversions
        // often drop metadata and only carry the data pointer address.
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
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
    ) {
        // Stack allocation lifetime: StorageLive/StorageDead.
        match stmt.kind {
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                if local != RETURN_PLACE
                    && (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                {
                    let live = matches!(stmt.kind, StatementKind::StorageLive(_));
                    let ty = body.local_decls[local].ty;

                    // Only record stack allocations for *pointee* locals (actual stack slots).
                    // Pointer-typed locals (`&T`, `*mut T`, `*const T`) are just pointer values; recording
                    // their addresses as allocations pollutes ALLOCS.
                    if !self.is_pointer_ty(ty) {
                        let size_op =
                            self.size_operand_for_ty(tcx, body, ty, stmt.source_info.span);
                        if !matches!(size_op, SizeOperand::Const(_)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc { local, live, size_op },
                            });
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
                        if self.is_thin_ptr_ty(tcx, ptr_ty) {
                            // Best-effort size: use the type of the *loaded place* (after projections).
                            // This is important for patterns where the destination is a projection
                            // (e.g., `_tmp = (*p).field`) or when the LHS is not a plain local.
                            let loaded_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                            let size_op =
                                self.size_operand_for_ty(tcx, body, loaded_ty, stmt.source_info.span);

                            self.ensure_raw_root_before(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                stmt.source_info,
                                ptr_local,
                                insert_points,
                                tagged_ptr_locals,
                            );
                            ptr_locals_needing_tag.insert(ptr_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(ptr_local),
                                kind: InstrKind::PtrRead { ptr_local, size_op },
                            });
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
                if self.is_thin_ptr_ty(tcx, ptr_ty) {
                    // Best-effort size: use the type of the *place being written* (after projections).
                    // This yields the correct size for patterns like `(*p).field = ...` or `(*p)[i] = ...`.
                    let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                    let size_op =
                        self.size_operand_for_ty(tcx, body, lhs_ty, stmt.source_info.span);

                    self.ensure_raw_root_before(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        ptr_local,
                        insert_points,
                        tagged_ptr_locals,
                    );
                    ptr_locals_needing_tag.insert(ptr_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: Place::from(ptr_local),
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
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local()),
                        // CopyForDeref shows up when MIR materializes a place for deref;
                        // it still represents a pointer value that needs tag propagation.
                        Rvalue::CopyForDeref(p) => p.as_local(),
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
                        if let Rvalue::Use(op) = rvalue {
                            if let Some(p) = self.place_from_operand(op) {
                                if p.projection.len() == 1 {
                                    if let ProjectionElem::Field(field, _ty) = p.projection[0] {
                                        let field_idx = field.index();
                                        src_local_opt = self.backtrack_aggregate_field_local(
                                            p.local,
                                            field_idx,
                                            &block_data.statements[..stmt_idx],
                                        );
                                    }
                                }
                            }
                        }
                    }

                    if let Some(src_local) = src_local_opt {
                        let src_ty = body.local_decls[src_local].ty;
                        if self.is_pointer_ty(src_ty) {
                            ptr_locals_needing_tag.insert(dst_local);
                            ptr_locals_needing_tag.insert(src_local);

                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::TagProp { dst: dst_local, src: src_local },
                            });
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
                            // Do not synthesize a tag here; the destination local is still
                            // being assigned, and inserting a RawRoot can read an uninitialized
                            // pointer value. Let the first PtrRead/PtrWrite/PtrUse insert a
                            // RawRoot after the assignment instead.
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
                                    if self.is_thin_ptr_ty(tcx, dst_ty) {
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

        //  std/alloc pattern where a thin pointer is produced by `Transmute` from
        // `NonNull<T>`/`Unique<T>` (ADT). TagProp does not apply because the source is not a thin
        // pointer local, so we synthesize a *root* raw-pointer tag for the destination.
        //
        // TODO: recover the parent tag from the pointer stored inside the ADT and propagate it.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_thin_ptr_ty(tcx, dst_ty) {
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
                if self.is_thin_ptr_ty(tcx, dst_ty) {
                    let (binop, lhs_op) = match rvalue {
                        Rvalue::BinaryOp(op, box (lhs, _rhs)) => (Some(*op), Some(lhs)),
                        // Newer nightlies no longer have `Rvalue::CheckedBinaryOp`. The checked/overflowing
                        // forms lower to regular `BinaryOp` + extra logic, so handling `BinaryOp` is enough
                        // for our pointer-derive tagging purposes here.
                        _ => (None, None),
                    };

                    if let (Some(op), Some(lhs)) = (binop, lhs_op) {
                        if matches!(op, BinOp::Add | BinOp::Sub) {
                            if let Some(src_place) = self.place_from_operand(lhs) {
                                let src_local = src_place.local;
                                let src_ty = body.local_decls[src_local].ty;
                                if self.is_thin_ptr_ty(tcx, src_ty) {
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
                                        kind: InstrKind::PtrDerive { dst: dst_local, src: src_local, is_mut },
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
                if self.is_pointer_ty(lhs_ty) {
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
        if !self.is_thin_ptr_ty(tcx, ptr_ty) {
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }

        let elem_ty = match ptr_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => *pointee_ty,
            TyKind::Ref(_, pointee_ty, _) => *pointee_ty,
            _ => {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
        };

        if !elem_ty.is_sized(tcx, body.typing_env(tcx)) {
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
    ) {
        if !self.warn_unknown_calls_enabled() {
            return;
        }

        if let Some(def_path) = callee_path_opt {
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
                let effect = self.classify_call_effect(def_path);

                let known = !matches!(effect, CallEffect::Unknown);
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
                let src_local = args.get(0).and_then(|a| self.place_from_operand(&a.node)).map(|p| p.local);
                let dst_local = args.get(1).and_then(|a| self.place_from_operand(&a.node)).map(|p| p.local);
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
                        place: Place::from(src),
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
                        place: Place::from(dst),
                        kind: InstrKind::PtrWrite { ptr_local: dst, size_op },
                    });
                }
            }
        } else if is_memset {
            // Signature convention:
            //   write_bytes::<T>(dst: *mut T, val: u8, count: usize)
            if args.len() >= 3 {
                let dst_local = args.get(0).and_then(|a| self.place_from_operand(&a.node)).map(|p| p.local);
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
                        place: Place::from(dst),
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
                    if self.is_thin_ptr_ty(tcx, dst_ty) {
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
                        if self.is_thin_ptr_ty(tcx, ptr_ty) {
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
                        if self.is_thin_ptr_ty(tcx, old_ptr_ty) {
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
                    if self.is_thin_ptr_ty(tcx, dst_ty) {
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

        // 6a: Remove is_plain_store/is_plain_load computation.

        // Centralized effect classification for direct calls.
        let mut call_effect_opt: Option<CallEffect> = callee_path_opt.as_deref().map(|p| self.classify_call_effect(p));
        let unknown_call = !callee_instrumented
            && matches!(call_effect_opt, None | Some(CallEffect::Unknown));

        // Warn when we see a *direct* call that likely has pointer-based memory effects,
        // but we failed to classify it as a known wrapper/intrinsic, and the callee is not instrumented.
        // This helps avoid silently missing std/core/dep wrappers.
        self.warn_unknown_call_if_needed(
            tcx,
            body,
            args,
            destination,
            callee_path_opt.as_deref(),
            callee_instrumented,
        );

        let mut classified_write_ptr_local: Option<Local> = None;
        let mut classified_read_ptr_local: Option<Local> = None;
        let mut classified_derive_ptr_local: Option<Local> = None;

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

                                if self.is_thin_ptr_ty(tcx, dst_ty) {
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
                            classified_write_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);

                            let ty0 = body.local_decls[p0.local].ty;
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
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrWrite {
                                    ptr_local: p0.local,
                                    size_op,
                                },
                            });
                        }
                    }
                }

                CallEffect::Load => {
                    // load wrapper/intrinsic: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            classified_read_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);

                            let ty0 = body.local_decls[p0.local].ty;
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
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrRead {
                                    ptr_local: p0.local,
                                    size_op,
                                },
                            });
                        }
                    }
                }

                CallEffect::PtrDerive => {
                    // Pointer-result handling for ptr-derivation wrappers (add/sub/offset/as_ptr...).
                    // Only needed when the callee is not instrumented.
                    if !callee_instrumented {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if self.is_thin_ptr_ty(tcx, dst_ty) {
                                // Find base pointer local in arg0 (thin ptr) or backtrack an unsize cast.
                                let mut src_local_opt: Option<Local> = None;
                                if let Some(first) = args.get(0) {
                                    if let Some(arg_place) = self.place_from_operand(&first.node) {
                                        let arg_local = arg_place.local;
                                        let arg_ty = body.local_decls[arg_local].ty;
                                        if self.is_pointer_ty(arg_ty) {
                                            src_local_opt = Some(arg_local);
                                        } else if let Some(base_local) =
                                            self.backtrack_unsize_base_local(arg_local, &block_data.statements)
                                        {
                                            let base_ty = body.local_decls[base_local].ty;
                                            if self.is_pointer_ty(base_ty) {
                                                src_local_opt = Some(base_local);
                                            }
                                        }
                                    }
                                }

                                if let Some(src_local) = src_local_opt {
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
                            if self.is_thin_ptr_ty(tcx, dst_ty) {
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


        // Treat any pointer argument as tag relevant, including wide pointers, so argument tags
        // survive through metadata carrying types that later yield thin data pointers.
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else { continue; };
            let ty = body.local_decls[p.local].ty;
            if !self.is_pointer_ty(ty) { continue; }
        
            // Inter-procedural: push argument tag to callee if instrumented.
            if callee_instrumented {
                if let Some(callee_id) = callee_id_opt {
                    ptr_locals_needing_tag.insert(p.local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(p.local),
                        kind: InstrKind::CallArgPush {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                        },
                    });
                }
            }

            // Unknown call policy: conservatively model potential read/write through any pointer arg.
            if unknown_call {
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
                    place: Place::from(p.local),
                    kind: InstrKind::PtrRead { ptr_local: p.local, size_op: size_op.clone() },
                });
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(p.local),
                    kind: InstrKind::PtrWrite { ptr_local: p.local, size_op },
                });
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
                place: Place::from(p.local),
                kind: InstrKind::PtrUse { ptr_local: p.local },
            });

            if !tagged_ptr_locals.contains(&p.local) {
                let is_mut = match ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                tagged_ptr_locals.insert(p.local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(p.local),
                    kind: InstrKind::RawRoot { ptr_local: p.local, is_mut },
                });
            }
        }

        // Caller-side return-tag recovery for pointer returns, including wide pointers whose
        // address is tracked through the data pointer.
        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.is_pointer_ty(dst_ty) {
                if callee_instrumented {
                    if let Some(callee_id) = callee_id_opt {
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
                    let return_tagged_by_effect = matches!(
                        call_effect_opt,
                        Some(CallEffect::PtrDerive | CallEffect::BoxIntoRaw)
                    ) || (alloc_returns_ptr && !self.heap_allocs_from_mir_enabled());

                    if !return_tagged_by_effect {
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
            }
        }
    }

    fn scan_body<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> ScanResult<'tcx> {
        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();
        let mut tagged_ptr_locals: HashSet<Local> = HashSet::new();

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
            if local == RETURN_PLACE {
                continue;
            }
            if explicitly_tracked.contains(&local) {
                continue;
            }
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) {
                continue;
            }
            let size_op = self.size_operand_for_ty(tcx, body, ty, rustc_span::DUMMY_SP);
            if matches!(size_op, SizeOperand::Const(_)) {
                continue;
            }
            fallback_locals.push((local, size_op));
        }

        let interesting_stack_locals = self.compute_interesting_stack_locals(tcx, body);
        let track_all_stack_allocs = self.track_all_stack_allocs_flag();
        eprintln!(
            "[rusteze][trace] track_all_stack_allocs={} RZ_STACK_ALLOCS={:?}",
            track_all_stack_allocs,
            std::env::var("RZ_STACK_ALLOCS").ok()
          );
          
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
                    if self.is_pointer_ty(body.return_ty()) {
                        let callee_id = self.callee_id_u64(tcx, body.source.def_id());
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
            if local == RETURN_PLACE {
                continue;
            }

            // Only track stack slots that are actually address-taken (unless user forces all).
            if !track_all_stack_allocs && !interesting_stack_locals.contains(&local) {
                continue;
            }

            // Never record pointer-typed locals as allocations.
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) {
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
            }
        }

        // Prepend entry fallback points so they are applied last and execute first.
        insert_points.splice(0..0, fallback_entry_points);
        // Append return points normally; they stay associated with return blocks.
        insert_points.extend(fallback_return_points);

        ScanResult { insert_points, ptr_locals_needing_tag }
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
            InstrKind::ConstAlloc { .. } => hooks.def_id_alloc,
            InstrKind::PtrWrite { .. } => hooks.def_id_write,
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::TagProp { .. } => hooks.def_id_use, // should never become a call (handled as a plain Assign)
            InstrKind::PtrDerive { .. } => hooks.def_id_raw,
            InstrKind::CallArgPush { .. } => hooks.def_id_push_call_arg_tag,
            InstrKind::ArgRetag { .. } => hooks.def_id_take_call_arg_tag,
            InstrKind::RetPush { .. } => hooks.def_id_push_ret_tag,
            InstrKind::RetTake { .. } => hooks.def_id_take_ret_tag,
        };
        Operand::function_handle(tcx, def_id, std::iter::empty(), sp)
    }

    fn insert_instrumentation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        insert_points: Vec<InsertPoint<'tcx>>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        hooks: Hooks,
    ) {
        for ip in insert_points.into_iter().rev() {
            let bb = ip.bb;
            let stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            // workaround for pointers produced from NonNull/Unique via Transmute
            // RawRoot lowering: we implement this by mirroring the existing Raw lowering code path:
            //   tag(ptr_local) = __record_raw_ptr_creation(expose(ptr_local), is_mut, 0)
            if let InstrKind::RawRoot { ptr_local, is_mut } = creation_kind.clone() {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RawRoot");

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

                // In the call block, compute exposed address.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                if let Some((data_ptr_stmt_opt, addr_stmt)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(ptr_local),
                    addr_local,
                ) {
                    if let Some(data_ptr_stmt) = data_ptr_stmt_opt {
                        body.basic_blocks_mut()[call_bb].statements.push(data_ptr_stmt);
                    }
                    body.basic_blocks_mut()[call_bb].statements.push(addr_stmt);
                } else {
                    body.basic_blocks_mut()[call_bb].statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(Place::from(ptr_local)),
                                tcx.types.usize,
                            ),
                        ))),
                    ));
                }

                let raw_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_raw,
                    std::iter::empty(),
                    source_info.span,
                );

                let is_mut_u8: u8 = if is_mut { 1 } else { 0 };
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

                // Done handling this insert point.
                continue;
            }

            // Caller-side: take return tag after a call returned a thin pointer into `dst_local`.
            // This must run after the call, so we rewrite the call's target to a fresh block that
            // performs `__rz_take_ret_tag` and then jumps to the original target.
            if let InstrKind::RetTake { callee_id, dst_local } = creation_kind {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing tag local for RetTake");

                let is_cleanup = body.basic_blocks[bb].is_cleanup;

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

                // New block that runs after the call returns.
                let take_bb = body.basic_blocks_mut().push(BasicBlockData::new(None, is_cleanup));

                // Redirect original call to take_bb.
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetTake");
                    if let TerminatorKind::Call { target, .. } = &mut term.kind {
                        *target = Some(take_bb);
                    }
                }

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
                    hooks.def_id_take_ret_tag,
                    std::iter::empty(),
                    source_info.span,
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
                ]
                .into_boxed_slice();

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let take_bd = &mut body.basic_blocks_mut()[take_bb];
                if let Some(addr_stmt1) = addr_stmt1_opt {
                    take_bd.statements.push(addr_stmt1);
                }
                take_bd.statements.push(addr_stmt2);
                take_bd.terminator = Some(take_term);

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

            if let InstrKind::TagProp { dst, src } = creation_kind {
                // println!(
                //     "[instrument-mir] TAG PROPAGATION: dst_local={:?} src_local={:?}",
                //     dst,
                //     src
                // );
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst)
                    .expect("missing tag local for TagProp dst");

                let src_op: Operand<'tcx> = if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
                    Operand::Copy(Place::from(*src_tag))
                } else {
                    self.const_u64(tcx, source_info.span, 0)
                };

                let prop_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_tag),
                        Rvalue::Use(src_op),
                    ))),
                );

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.insert(insert_at, prop_stmt);
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
                    bd.terminator = Some(take_term);
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

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

            let func_operand = self.func_operand_for(tcx, hooks, &creation_kind, source_info.span);

            let insert_before: bool = ip.insert_before
                || matches!(
                    &creation_kind,
                    InstrKind::PtrRead { .. } | InstrKind::PtrWrite { .. }
                );

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
            let (addr_stmt1_opt, addr_stmt2) = match creation_kind {
                InstrKind::StackAlloc { local, .. } => {
                    let local_ty = body.local_decls[local].ty;
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
                _ => {
                    match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                        Some(stmts) => stmts,
                        None => continue,
                    }
                }
            };

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc { ptr_local, live, .. } => Some((*ptr_local, *live)),
                _ => None,
            };

            // For const/global allocations, rewrite the exposed pointer address to the base.
            let mut arg_addr_local = addr_local;
            let mut addr_adjust_stmt_opt: Option<Statement<'tcx>> = None;

            if let InstrKind::ConstAlloc { base_offset, .. } = &creation_kind {
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

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead { ptr_local, ref size_op } => {
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
                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

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
                InstrKind::ConstAlloc { size, .. } => {
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

                InstrKind::PtrWrite { ptr_local, ref size_op } => {
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

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
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

                InstrKind::PtrDerive { dst, src, is_mut } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDerive dst");

                    let parent_tag_op: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_mut, span: source_info.span },
                        Spanned { node: parent_tag_op, span: source_info.span },
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
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            let base = src.local;
                            if let Some(tl) = tag_local_for_ptr_local.get(&base) {
                                Operand::Copy(Place::from(*tl))
                            } else {
                                self.const_u64(tcx, source_info.span, 0)
                            }
                        }
                        _ => self.const_u64(tcx, source_info.span, 0),
                    };

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_mut, span: source_info.span },
                        Spanned { node: arg_parent, span: source_info.span },
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

            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
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

    pub(crate) fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let def_id = body.source.def_id();
        let def_path = tcx.def_path_str(def_id);

        if def_path.contains("runtime") {
            println!("Skipping optimization for {}", def_path);
            return;
        }

        let crate_name = tcx.crate_name(def_id.krate);

        // Skip the `runtime` crate
        if crate_name.as_str() == "runtime" {
            println!(
                "Skipping optimization for item in runtime crate: {:?}",
                def_id
            );
            return;
        }

        println!(
            "Running MyOptimizationPass on {:?} {:?}",
            body.source.def_id(),
            def_path
        );


        // self.print_runtime_items(tcx);

        let def_id_ref = self
            .find_def_id_by_name(tcx, "__record_ref_creation")
            .expect("missing '__record_ref_creation' definition");
        let def_id_raw = self
            .find_def_id_by_name(tcx, "__record_raw_ptr_creation")
            .expect("missing '__record_raw_ptr_creation' definition");
        let def_id_alloc = self
            .find_def_id_by_name(tcx, "__rz_record_alloc")
            .expect("missing '__rz_record_alloc' definition");
        let def_id_write = self
            .find_def_id_by_name(tcx, "__rz_ptr_write")
            .expect("missing '__rz_ptr_write' definition");
        let def_id_read = self
            .find_def_id_by_name(tcx, "__rz_ptr_read")
            .expect("missing '__rz_ptr_read' definition");
        let def_id_use = self
            .find_def_id_by_name(tcx, "__rz_ptr_use")
            .expect("missing '__rz_ptr_use' definition");
        let def_id_push_call_arg_tag = self
            .find_def_id_by_name(tcx, "__rz_push_call_arg_tag")
            .expect("missing '__rz_push_call_arg_tag' definition");
        let def_id_take_call_arg_tag = self
            .find_def_id_by_name(tcx, "__rz_take_call_arg_tag")
            .expect("missing '__rz_take_call_arg_tag' definition");
        let def_id_push_ret_tag = self
            .find_def_id_by_name(tcx, "__rz_push_ret_tag")
            .expect("missing '__rz_push_ret_tag' definition");
        let def_id_take_ret_tag = self
            .find_def_id_by_name(tcx, "__rz_take_ret_tag")
            .expect("missing '__rz_take_ret_tag' definition");

        let hooks = Hooks {
            def_id_ref,
            def_id_raw,
            def_id_alloc,
            def_id_write,
            def_id_read,
            def_id_use,
            def_id_push_call_arg_tag,
            def_id_take_call_arg_tag,
            def_id_push_ret_tag,
            def_id_take_ret_tag,
        };

        let scan = self.scan_body(tcx, body);
        let tag_local_for_ptr_local =
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
    }
}
