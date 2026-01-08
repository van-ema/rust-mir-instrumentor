use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::Mutability;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::{PseudoCanonicalInput, Ty, TyCtxt, TypingEnv};
use rustc_middle::ty::TyKind;
use rustc_span::{source_map::Spanned, Span};

pub(crate) struct MyOptimizationPass;

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
    /// Non-volatile ptr::read*/write* wrappers.
    PlainLoad,
    PlainStore,
    /// Volatile wrappers / intrinsics.
    VolatileLoad,
    VolatileStore,
    /// Pointer derivation wrappers that return a pointer derived from arg0 (fresh tag, parent linkage).
    PtrDerive,
    /// Box boundary modeling (Option B).
    BoxIntoRaw,
    BoxFromRaw,
    /// Allocator shims/wrappers.
    AllocShim(AllocShimKind),
    /// Not recognized.
    Unknown,
}

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
enum InstrKind<'tcx> {
    Ref { bk: BorrowKind, src: Place<'tcx> },
    Raw { is_mut: bool, src: Place<'tcx> },
    /// Root raw pointer creation for a pointer value already computed in a local.
    /// std/alloc often stores pointers inside ADTs like `NonNull<T>`/`Unique<T>` and then
    /// produces a thin pointer via `Transmute`. Our TagProp only propagates between thin pointer
    /// locals, so without this the destination pointer keeps tag=0 and triggers UNKNOWN_TAG.
    RawRoot { ptr_local: Local, is_mut: bool },
    /// Stack allocation lifetime event for a MIR local.
    StackAlloc { local: Local, live: bool, size: usize },
    /// Heap allocation lifetime event for an allocator-returned pointer.
    /// `ptr_local` holds the pointer value; `size_op` is the allocation size operand (usize).
    HeapAlloc { ptr_local: Local, live: bool, size_op: Operand<'tcx> },
    /// A write through a pointer local.
    /// `size` is best-effort (0 = unknown).
    PtrWrite { ptr_local: Local, size: usize },
    /// A read through a pointer local.
    /// `size` is best-effort (0 = unknown).
    PtrRead { ptr_local: Local, size: usize },
    /// Coarse pointer-use work tracking: a pointer-typed local appears in a call argument.
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
    /// Return true only for *thin* pointers (single-word), i.e. `&T` / `*const T` / `*mut T`
    /// where the pointer value is a single scalar. Fat pointers like `&[T]`, `&str`, and trait
    /// objects carry metadata and lower to a ScalarPair; casting them with
    /// `PointerExposeProvenance` currently triggers an ICE in codegen.
    fn is_thin_ptr_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(..) | TyKind::RawPtr(..) => {
                let ptr_bytes = tcx.data_layout.pointer_size().bytes() as usize;
                // Use layout size of the pointer type itself: thin ptr == pointer size; fat ptr == 2*ptr size (on 64-bit).
                self.layout_size_bytes(tcx, ty) == ptr_bytes
            }
            _ => false,
        }
    }

    /// Whether to warn about unknown (unclassified) direct calls that may read/write memory via pointers.
    /// Default: enabled. Set `RZ_WARN_UNKNOWN_CALLS=0` to disable.
    fn warn_unknown_calls_enabled(&self) -> bool {
        std::env::var("RZ_WARN_UNKNOWN_CALLS")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
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

    fn callee_id_u64(&self, def_id: DefId) -> u64 {
        ((def_id.krate.as_u32() as u64) << 32) | (def_id.index.as_u32() as u64)
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

            // Priority 2: optional "instrument all deps" mode.
            let instrument_all_deps = std::env::var("RZ_INSTRUMENT_ALL_DEPS")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if !instrument_all_deps {
                // Default: only current crate is assumed instrumented,
                return HashSet::new();
            }

            // Priority 3: instrument all non-std-like dependencies.
            let mut set = HashSet::new();
            for &cnum in tcx.crates(()).iter() {
                let name = tcx.crate_name(cnum).as_str().to_string();
                if name == "runtime" {
                    continue;
                }
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
        eprintln!("[rusteze] note: current crate is always treated as instrumented; set RZ_INSTRUMENTED_CRATES or RZ_INSTRUMENT_ALL_DEPS=1 to include deps.");
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

    fn layout_size_bytes<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> usize {
        let input = PseudoCanonicalInput {
            typing_env: TypingEnv::fully_monomorphized(),
            value: ty,
        };
        tcx.layout_of(input)
            .ok()
            .map(|l| l.size.bytes() as usize)
            .unwrap_or(0)
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
                                interesting.insert(src_place.local);
                            }
                        }
                        Rvalue::RawPtr(_mutbl, src_place) => {
                            if src_place.local != RETURN_PLACE {
                                interesting.insert(src_place.local);
                            }
                        }
                        _ => {}
                    }
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
        entry_stmt_idx: usize,
    ) {
        let entry_bb = START_BLOCK;
        let entry_source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };
        let callee_id = self.callee_id_u64(body.source.def_id());

        for (arg_index, arg_local) in body.args_iter().enumerate() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_thin_ptr_ty(tcx, arg_ty) {
                ptr_locals_needing_tag.insert(arg_local);
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
                    if !self.is_thin_ptr_ty(tcx, ty) {
                        let size = self.layout_size_bytes(tcx, ty);

                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(local),
                            kind: InstrKind::StackAlloc { local, live, size },
                        });
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
                        let ptr_local = p.local;

                        // Best-effort size: use the destination local's type size (0 if unknown).
                        let lhs_ty = body.local_decls[lhs_place.local].ty;
                        let size = self.layout_size_bytes(tcx, lhs_ty);

                        ptr_locals_needing_tag.insert(ptr_local);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(ptr_local),
                            kind: InstrKind::PtrRead { ptr_local, size },
                        });
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

                // Best-effort size: use the type of the *place being written* (after projections).
                // This yields the correct size for patterns like `(*p).field = ...` or `(*p)[i] = ...`.
                let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                let size = self.layout_size_bytes(tcx, lhs_ty);

                ptr_locals_needing_tag.insert(ptr_local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: Place::from(ptr_local),
                    kind: InstrKind::PtrWrite { ptr_local, size },
                });
            }
        }

        // Tag propagation across pointer-to-pointer casts and plain copies/moves of pointer locals.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_thin_ptr_ty(tcx, dst_ty) {
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self
                            .place_from_operand(op)
                            .and_then(|p| p.as_local()),
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

                    if let Some(src_local) = src_local_opt {
                        let src_ty = body.local_decls[src_local].ty;
                        if self.is_thin_ptr_ty(tcx, src_ty) {
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
                ptr_locals_needing_tag.insert(lhs_local);
            }
            insert_points.push(InsertPoint {
                bb,
                stmt_idx,
                insert_before: false,
                source_info: stmt.source_info,
                place: place.clone(),
                kind: InstrKind::Ref { bk: *bk, src: src_place.clone() },
            });
        }

        // Raw pointer creation
        if let StatementKind::Assign(box (place, Rvalue::RawPtr(mutbl, src_place))) = &stmt.kind {
            if let Some(lhs_local) = place.as_local() {
                ptr_locals_needing_tag.insert(lhs_local);
            }
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


    /// Recognize `core/std::ptr::{read_volatile,write_volatile}` wrappers.
    ///
    /// We want to treat these as READ/WRITE even before they inline down to
    /// `core::intrinsics::{volatile_load,volatile_store}`.
    fn classify_std_ptr_volatile_wrapper(&self, def_path: &str) -> (bool, bool) {
        let mut is_store = false;
        let mut is_load = false;

        // Examples of def_path_str():
        //   "core::ptr::read_volatile"
        //   "std::ptr::write_volatile"
        //   "core::ptr::read_volatile::<i32>"
        //   "std::ptr::write_volatile::<u64>"
        //
        // We keep this purely string-based so it works uniformly across inlining/monomorphization.
        if def_path.contains("::ptr::read_volatile") {
            is_load = true;
        } else if def_path.contains("::ptr::write_volatile") {
            is_store = true;
        }

        (is_store, is_load)
    }

    /// Recognize non-volatile std/core ptr load/store wrappers.
    ///
    /// These are *memory effects* even though the MIR often does not contain an explicit `(*p)`.
    /// We classify them so we can emit PtrRead/PtrWrite hooks.
    ///
    /// Covered (monomorphized paths included):
    ///   - core::ptr::read / std::ptr::read
    ///   - core::ptr::read_unaligned / std::ptr::read_unaligned
    ///   - core::ptr::write / std::ptr::write
    ///   - core::ptr::write_unaligned / std::ptr::write_unaligned
    fn classify_std_ptr_plain_wrapper(&self, def_path: &str) -> (bool, bool) {
        let mut is_store = false;
        let mut is_load = false;

        // Examples of def_path_str():
        //   "core::ptr::read"
        //   "std::ptr::write"
        //   "core::ptr::read_unaligned::<u64>"
        //   "std::ptr::write_unaligned::<i32>"
        if def_path.contains("::ptr::read_unaligned") {
            is_load = true;
        } else if def_path.contains("::ptr::read") {
            is_load = true;
        } else if def_path.contains("::ptr::write_unaligned") {
            is_store = true;
        } else if def_path.contains("::ptr::write") {
            is_store = true;
        }

        (is_store, is_load)
    }

    /// Recognize memcpy/memmove/memset-like intrinsics and thin std/core wrappers.
    /// Returns (is_memcpy, is_memset).
    fn classify_mem_intrinsic_or_wrapper(&self, def_path: &str) -> (bool, bool) {
        // Intrinsics:
        //   core::intrinsics::copy
        //   core::intrinsics::copy_nonoverlapping
        //   core::intrinsics::write_bytes
        //
        // Wrappers (common free functions):
        //   core::ptr::copy
        //   core::ptr::copy_nonoverlapping
        //   core::ptr::write_bytes
        //   std::ptr::copy
        //   std::ptr::copy_nonoverlapping
        //   std::ptr::write_bytes
        //
        // Wrappers (method-style, seen in MIR as monomorphized impl methods):
        //   std::ptr::mut_ptr::<impl *mut T>::write_bytes
        //   core::ptr::mut_ptr::<impl *mut T>::write_bytes
        //   ...::<impl *mut T>::copy / copy_nonoverlapping (rare but possible)

        let is_intr_copy = def_path.contains("::intrinsics::copy")
            || def_path.contains("::intrinsics::copy_nonoverlapping");

        // Free-function wrappers.
        let is_ptr_copy_fn = def_path.contains("::ptr::copy")
            || def_path.contains("::ptr::copy_nonoverlapping");
        let is_ptr_memset_fn = def_path.contains("::ptr::write_bytes");

        // Method-style wrappers: any def_path under a ptr module ending in these names.
        let is_ptr_copy_method = def_path.contains("::ptr::")
            && (def_path.ends_with("::copy") || def_path.ends_with("::copy_nonoverlapping"));
        let is_ptr_memset_method =
            def_path.contains("::ptr::") && def_path.ends_with("::write_bytes");

        let is_copy = is_intr_copy || is_ptr_copy_fn || is_ptr_copy_method;
        let is_memset =
            def_path.contains("::intrinsics::write_bytes") || is_ptr_memset_fn || is_ptr_memset_method;
        (is_copy, is_memset)
    }

    /// Recognize Rust allocator shims and common alloc::alloc wrappers (like exchange_malloc, alloc, etc.)
    /// that back `Box`, `Vec`, etc. We instrument these to populate the runtime allocation map.
    fn classify_rust_allocator_shim(&self, def_path: &str) -> AllocShimKind {
        // NOTE: std/core/alloc are typically NOT instrumented by this pass, even in "instrument all deps" mode.
        // Heap allocations for Vec/Box therefore frequently appear as calls to alloc wrappers like
        // `alloc::alloc::exchange_malloc` rather than the raw `__rust_alloc` shims.

        // Low-level shims (paths can be "__rust_alloc" or "...::__rust_alloc").
        if def_path.contains("__rust_alloc_zeroed") {
            return AllocShimKind::AllocZeroed;
        }
        if def_path.contains("__rust_alloc") {
            return AllocShimKind::Alloc;
        }
        if def_path.contains("__rust_dealloc") {
            return AllocShimKind::Dealloc;
        }
        if def_path.contains("__rust_realloc") {
            return AllocShimKind::Realloc;
        }

        // alloc::alloc wrappers commonly seen in MIR (especially optimized builds).
        // - exchange_malloc(size, align) -> *mut u8
        // - alloc(size, align) -> *mut u8
        // - alloc_zeroed(size, align) -> *mut u8
        // - dealloc(ptr, size, align)
        // - realloc(ptr, old_size, align, new_size) -> *mut u8
        if def_path.contains("alloc::alloc::exchange_malloc") {
            return AllocShimKind::Alloc;
        }
        if def_path.contains("alloc::alloc::alloc_zeroed") {
            return AllocShimKind::AllocZeroed;
        }
        // Keep this after alloc_zeroed so it doesn't catch it first.
        if def_path.contains("alloc::alloc::alloc") {
            return AllocShimKind::Alloc;
        }
        if def_path.contains("alloc::alloc::dealloc") {
            return AllocShimKind::Dealloc;
        }
        if def_path.contains("alloc::alloc::realloc") {
            return AllocShimKind::Realloc;
        }

        // std::alloc wrappers (often take `Layout` instead of (size, align)).
        // We still instrument them so heap liveness/epoch tracking works when std/core are not instrumented.
        // TODO: extract Layout.size so we can do precise OOB for std::alloc::{alloc,dealloc,realloc}.
        if def_path.contains("std::alloc::alloc_zeroed") {
            return AllocShimKind::AllocZeroed;
        }
        if def_path.contains("std::alloc::alloc") {
            return AllocShimKind::Alloc;
        }
        if def_path.contains("std::alloc::dealloc") {
            return AllocShimKind::Dealloc;
        }
        if def_path.contains("std::alloc::realloc") {
            return AllocShimKind::Realloc;
        }

        AllocShimKind::No
    }

    /// Centralized call-effect classifier ("table").
    ///
    /// This MUST be kept consistent with instrumentation emission so that
    /// `warn_unknown_call_if_needed` does not drift from actual handling.
    fn classify_call_effect(&self, def_path: &str) -> CallEffect {
        // 1) Allocator shims/wrappers.
        let ak = self.classify_rust_allocator_shim(def_path);
        if ak != AllocShimKind::No {
            return CallEffect::AllocShim(ak);
        }

        // 2) Box wrappers.
        if self.is_box_into_raw_wrapper(def_path) {
            return CallEffect::BoxIntoRaw;
        }
        if self.is_box_from_raw_wrapper(def_path) {
            return CallEffect::BoxFromRaw;
        }

        // 3) Pointer derivation wrappers.
        if self.is_std_ptr_derive_wrapper(def_path) {
            return CallEffect::PtrDerive;
        }

        // 4) Volatile wrappers (string-based; note that `is_volatile()` also detects intrinsics by item_name).
        let (vs, vl) = self.classify_std_ptr_volatile_wrapper(def_path);
        if vs {
            return CallEffect::VolatileStore;
        }
        if vl {
            return CallEffect::VolatileLoad;
        }

        // 5) Plain ptr load/store wrappers.
        let (ps, pl) = self.classify_std_ptr_plain_wrapper(def_path);
        if ps {
            return CallEffect::PlainStore;
        }
        if pl {
            return CallEffect::PlainLoad;
        }

        // 6) Memcpy/memset-like operations.
        let (is_copy, is_memset) = self.classify_mem_intrinsic_or_wrapper(def_path);
        if is_copy {
            return CallEffect::MemCopy;
        }
        if is_memset {
            return CallEffect::MemSet;
        }

        // 7) No-op / value-level helpers.
        // `ptr::is_null` does not read/write memory.
        if def_path.contains("::ptr::") && def_path.ends_with("::is_null") {
            return CallEffect::Ignore;
        }

        CallEffect::Unknown
    }

    /// Best-effort: compute byte size for memory ops given a pointer operand local and a count operand.
    /// If count is not a constant or pointee size is unknown, returns 0.
    fn memop_size_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        count_op: &Operand<'tcx>,
    ) -> usize {
        let pointee_size = match body.local_decls[ptr_local].ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            TyKind::Ref(_, pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            _ => 0,
        };
        if pointee_size == 0 {
            return 0;
        }

        // Try to extract constant count.
        let count: Option<u64> = match count_op {
            Operand::Constant(c) => {
                match c.const_.try_to_scalar() {
                    Some(s) => s.to_u64().discard_err(),
                    None => None,
                }
            }
            _ => None,
        };

        count
            .and_then(|c| (c as usize).checked_mul(pointee_size))
            .unwrap_or(0)
    }

    /// Recognize std/core pointer-derivation wrappers that return a pointer derived from
    /// a base pointer argument (typically arg0). We treat these like pointer arithmetic,
    /// i.e. the result gets a fresh tag derived from the base tag.
    ///
    /// Expanded to include slice/Vec pointer-extraction wrappers, which also derive a pointer from a fat pointer.
    fn is_std_ptr_derive_wrapper(&self, def_path: &str) -> bool {
        // Pointer-derivation wrappers: these produce a pointer derived from a base pointer/slice.
        // We treat them like pointer arithmetic (fresh tag with parent linkage).
        //
        // Common pointer arithmetic wrappers (monomorphized):
        //   "core::ptr::const_ptr::<impl *const T>::add"
        //   "core::ptr::mut_ptr::<impl *mut T>::offset"
        //   "std::ptr::const_ptr::<impl *const T>::wrapping_add"
        //   "core::ptr::wrapping_offset" (older paths)
        let is_ptr_arith = def_path.contains("::ptr::")
            && (def_path.contains("::add")
                || def_path.contains("::sub")
                || def_path.contains("::offset")
                || def_path.contains("::wrapping_add")
                || def_path.contains("::wrapping_sub")
                || def_path.contains("::wrapping_offset")
                || def_path.contains("::byte_add")
                || def_path.contains("::byte_sub")
                || def_path.contains("::wrapping_byte_add")
                || def_path.contains("::wrapping_byte_sub"));

        // Slice/Vec pointer extraction wrappers (these also *derive* a pointer from a fat pointer):
        //   "core::slice::<impl [T]>::as_ptr"
        //   "core::slice::<impl [T]>::as_mut_ptr"
        //   "alloc::vec::Vec::<T>::as_ptr"
        //   "alloc::vec::Vec::<T>::as_mut_ptr"
        // Note: we keep this string-based to work across monomorphization/inlining.
        let is_slice_ptr = def_path.contains("::slice::<impl [")
            && (def_path.contains("::as_ptr") || def_path.contains("::as_mut_ptr"));

        let is_vec_ptr = def_path.contains("alloc::vec::Vec")
            && (def_path.contains("::as_ptr") || def_path.contains("::as_mut_ptr"));

        is_ptr_arith || is_slice_ptr || is_vec_ptr
    }

    /// Recognize std/alloc Box wrappers that return a raw pointer but take an ADT (Box<T>) as input.
    ///
    /// In optimized MIR, `Box::into_raw` appears as a direct call where the argument is an ADT,
    /// so we cannot use TagProp (arg0 is not a thin pointer local). We therefore synthesize a root
    /// raw-pointer tag for the returned pointer local (Option B).
    fn is_box_into_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::into_raw")
    }

    fn is_box_from_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::from_raw")
    }

    fn is_volatile<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        func: &Operand<'tcx>,
    ) -> (bool, bool) {
        let mut is_volatile_store = false;
        let mut is_volatile_load = false;

        if let TyKind::FnDef(callee_def_id, _) = func.ty(body, tcx).kind() {
            // Fast path for intrinsics (these are the "real" volatile ops once inlined).
            let path = tcx.def_path_str(*callee_def_id);
            if path.starts_with("core::intrinsics::") || path.starts_with("std::intrinsics::") {
                let name_sym: rustc_span::symbol::Symbol = tcx.item_name(*callee_def_id);
                match name_sym.as_str() {
                    "volatile_store" => is_volatile_store = true,
                    "volatile_load" => is_volatile_load = true,
                    _ => {}
                }
                return (is_volatile_store, is_volatile_load);
            }
            (is_volatile_store, is_volatile_load) = self.classify_std_ptr_volatile_wrapper(&path);
        }

        (is_volatile_store, is_volatile_load)
    }

    fn direct_callee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        func: &Operand<'tcx>,
    ) -> Option<(DefId, u64)> {
        if let TyKind::FnDef(callee_def_id, _) = func.ty(body, tcx).kind() {
            let cid = self.callee_id_u64(*callee_def_id);
            return Some((*callee_def_id, cid));
        }
        None
    }

    fn push_ptr_derive_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        src_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
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

        // IMPORTANT: for ptr-derivation wrappers (add/sub/offset/...), the destination local
        // is only initialized *after* the call returns. We must therefore insert the PtrDerive
        // hook in the call's `target` block, not in the call block itself, otherwise we
        // expose provenance of an uninitialized local and record a garbage pointee address.
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
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        // Special-case: Box::into_raw returns a thin pointer derived from a Box ADT argument.
        // Since arg0 is not a thin pointer local, TagProp cannot apply; synthesize a root tag.
        let is_mut = match dst_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        };

        // Best-effort heap range recording for Box<T>: the raw pointer points to the T allocation.
        // TODO(Option A): hook real allocator shims/drop glue to get exact layout/size in general.
        let pointee_size: usize = match dst_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            TyKind::Ref(_, pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            _ => 0,
        };
        let size_op: Operand<'tcx> = self.const_usize(tcx, term.source_info.span, pointee_size);

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

    fn push_box_from_raw_call<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        ptr_local: Local,
        ptr_ty: Ty<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        let pointee_size: usize = match ptr_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            TyKind::Ref(_, pointee_ty, _) => self.layout_size_bytes(tcx, *pointee_ty),
            _ => 0,
        };
        let size_op: Operand<'tcx> = self.const_usize(tcx, term.source_info.span, pointee_size);

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

    fn warn_unknown_call_if_needed<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        callee_path_opt: Option<&str>,
        callee_instrumented: bool,
        is_volatile_store: bool,
        is_volatile_load: bool,
        is_plain_store: bool,
        is_plain_load: bool,
    ) {
        if !self.warn_unknown_calls_enabled() {
            return;
        }

        if let Some(def_path) = callee_path_opt {
            // Does the call take any thin pointer argument?
            let mut has_ptr_arg = false;
            for a in args.iter() {
                if let Some(p) = self.place_from_operand(&a.node) {
                    let ty = body.local_decls[p.local].ty;
                    if self.is_thin_ptr_ty(tcx, ty) {
                        has_ptr_arg = true;
                        break;
                    }
                }
            }

            // Does the call return a thin pointer into a local?
            let returns_ptr = destination
                .as_local()
                .is_some_and(|dl| self.is_thin_ptr_ty(tcx, body.local_decls[dl].ty));

            if (has_ptr_arg || returns_ptr) && !callee_instrumented {
                // Use the centralized classifier so warning suppression matches actual handling.
                let effect = self.classify_call_effect(def_path);

                // Also treat volatile/plain wrapper flags (computed earlier) as known.
                // `is_volatile()` can classify intrinsics via item_name even when def_path is generic.
                let known = is_volatile_store
                    || is_volatile_load
                    || is_plain_store
                    || is_plain_load
                    || !matches!(effect, CallEffect::Unknown);

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

                if let Some(src) = src_local {
                    *classified_read_ptr_local = Some(src);
                    ptr_locals_needing_tag.insert(src);
                    let size = self.memop_size_bytes(tcx, body, src, count_op);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(src),
                        kind: InstrKind::PtrRead { ptr_local: src, size },
                    });
                }
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size = self.memop_size_bytes(tcx, body, dst, count_op);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst),
                        kind: InstrKind::PtrWrite { ptr_local: dst, size },
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
                    let size = self.memop_size_bytes(tcx, body, dst, count_op);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst),
                        kind: InstrKind::PtrWrite { ptr_local: dst, size },
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
                        // Insert in the target block so `dst_local` is initialized.
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
    ) {
        let (is_volatile_store, is_volatile_load) = self.is_volatile(tcx, body, func);
        let callee_opt = self.direct_callee(tcx, body, func);
        let callee_id_opt = callee_opt.map(|(_did, cid)| cid);
        let callee_path_opt = callee_opt.map(|(did, _)| tcx.def_path_str(did));
        let callee_instrumented = callee_opt
            .map(|(did, _)| self.is_instrumented_callee(tcx, did))
            .unwrap_or(false);

        let mut is_plain_store = false;
        let mut is_plain_load = false;
        if let Some(path) = callee_path_opt.as_deref() {
            (is_plain_store, is_plain_load) = self.classify_std_ptr_plain_wrapper(path);
        }

        // Centralized effect classification for direct calls.
        let call_effect_opt: Option<CallEffect> = callee_path_opt.as_deref().map(|p| self.classify_call_effect(p));

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
            is_volatile_store,
            is_volatile_load,
            is_plain_store,
            is_plain_load,
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
                    // Allocator shims/wrappers: emit HeapAlloc live/dead events.
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
                    );
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

                CallEffect::VolatileStore => {
                    // Volatile store: WRITE through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            classified_write_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);
                
                            let mut size = 0usize;
                            let ty0 = body.local_decls[p0.local].ty;
                            if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                size = self.layout_size_bytes(tcx, *pointee_ty);
                            }
                
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrWrite { ptr_local: p0.local, size },
                            });
                        }
                    }
                }
                
                CallEffect::VolatileLoad => {
                    // Volatile load: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            classified_read_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);
                
                            let mut size = 0usize;
                            let ty0 = body.local_decls[p0.local].ty;
                            if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                size = self.layout_size_bytes(tcx, *pointee_ty);
                            }
                
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrRead { ptr_local: p0.local, size },
                            });
                        }
                    }
                }
                
                CallEffect::PlainStore => {
                    // Non-volatile ptr::write* wrappers: WRITE through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            classified_write_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);
                
                            let mut size = 0usize;
                            let ty0 = body.local_decls[p0.local].ty;
                            if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                size = self.layout_size_bytes(tcx, *pointee_ty);
                            }
                
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrWrite { ptr_local: p0.local, size },
                            });
                        }
                    }
                }
                
                CallEffect::PlainLoad => {
                    // Non-volatile ptr::read* wrappers: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            classified_read_ptr_local = Some(p0.local);
                            ptr_locals_needing_tag.insert(p0.local);
                
                            let mut size = 0usize;
                            let ty0 = body.local_decls[p0.local].ty;
                            if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                size = self.layout_size_bytes(tcx, *pointee_ty);
                            }
                
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(p0.local),
                                kind: InstrKind::PtrRead { ptr_local: p0.local, size },
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
                                        if self.is_thin_ptr_ty(tcx, arg_ty) {
                                            src_local_opt = Some(arg_local);
                                        } else if let Some(base_local) =
                                            self.backtrack_unsize_base_local(arg_local, &block_data.statements)
                                        {
                                            let base_ty = body.local_decls[base_local].ty;
                                            if self.is_thin_ptr_ty(tcx, base_ty) {
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
                                        &mut classified_derive_ptr_local,
                                    );
                                }
                            }
                        }
                    }
                }

                CallEffect::BoxIntoRaw => {
                    // Box::into_raw boundary modeling (Option B): root-tag + HeapAlloc live.
                    if !callee_instrumented {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if self.is_thin_ptr_ty(tcx, dst_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                self.push_box_into_raw_call(
                                    tcx,
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
                    // Conservative modeling: mark Box<T> allocation dead at Box::from_raw(ptr).
                    // Only when the callee is not instrumented.
                    if !callee_instrumented {
                        if let Some(first) = args.get(0) {
                            if let Some(p) = self.place_from_operand(&first.node) {
                                let ptr_local = p.local;
                                let ptr_ty = body.local_decls[ptr_local].ty;
                                if self.is_thin_ptr_ty(tcx, ptr_ty) {
                                    ptr_locals_needing_tag.insert(ptr_local);
                                    self.push_box_from_raw_call(
                                        tcx,
                                        bb,
                                        block_data,
                                        term,
                                        ptr_local,
                                        ptr_ty,
                                        insert_points,
                                    );
                                }
                            }
                        }
                    }
                }

                CallEffect::Unknown => {
                    // No special emission here.
                }
            }
        }


        let arg_is_already_accounted_for = |l: Local| {
            classified_write_ptr_local == Some(l)
                || classified_read_ptr_local == Some(l)
                || classified_derive_ptr_local == Some(l)
        };
        
        let suppress_ptr_use_for_call = matches!(
            call_effect_opt,
            Some(
                CallEffect::MemCopy
                    | CallEffect::MemSet
                    | CallEffect::PlainLoad
                    | CallEffect::PlainStore
                    | CallEffect::VolatileLoad
                    | CallEffect::VolatileStore
                    | CallEffect::PtrDerive
            )
        );
        
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else { continue; };
            let ty = body.local_decls[p.local].ty;
            if !self.is_thin_ptr_ty(tcx, ty) { continue; }
        
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
        
            if arg_is_already_accounted_for(p.local) {
                continue;
            }
        
            if suppress_ptr_use_for_call {
                continue;
            }
        
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
        }

        // Caller-side return-tag recovery: if the call returns a thin pointer into a local, take the tag.
        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.is_thin_ptr_ty(tcx, dst_ty) {
                if callee_instrumented {
                    if let Some(callee_id) = callee_id_opt {
                        ptr_locals_needing_tag.insert(dst_local);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::RetTake { callee_id, dst_local },
                        });
                    }
                }
            }
        }
    }

    fn scan_body<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> ScanResult<'tcx> {
        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();

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

        let mut arg_locals: HashSet<Local> = HashSet::new();
        for arg_local in body.args_iter() {
            arg_locals.insert(arg_local);
        }

        // Fallback stack locals:
        // Some locals never get explicit `StorageLive/StorageDead` in optimized MIR
        // (e.g. temporaries or values kept live for the whole function).
        // If such a local is used to create or derive a pointer (including via calls),
        // we would otherwise never record an allocation epoch for it, which causes
        // use-after-dead and stale-pointer checks to silently miss.
        // 
        // To handle this, we conservatively treat these locals as "always-live":
        //  - record a StackAlloc(live=true) at function entry
        //  - record a StackAlloc(live=false) at every return site
        //
        // This is a fallback mechanism; precise lifetime tracking via explicit
        // StorageLive/StorageDead takes precedence when available.
        let mut fallback_locals: Vec<(Local, usize)> = Vec::new();
        for local in body.local_decls.indices() {
            if local == RETURN_PLACE {
                continue;
            }
            if arg_locals.contains(&local) {
                continue;
            }
            if explicitly_tracked.contains(&local) {
                continue;
            }
            let ty = body.local_decls[local].ty;
            let size = self.layout_size_bytes(tcx, ty);
            if size == 0 {
                continue;
            }
            fallback_locals.push((local, size));
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
            entry_insert_at,
        );

        let entry_source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };
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
                    );
                }

                if let TerminatorKind::Return = &term.kind {
                    if self.is_thin_ptr_ty(tcx, body.return_ty()) {
                        let callee_id = self.callee_id_u64(body.source.def_id());
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
        // If we insert multiple terminator-splitting hooks at the same location, the *last applied*
        // hook will execute *first*.
        //
        // `insert_instrumentation` iterates `insert_points` in reverse, meaning:
        //   - earlier items in `insert_points` are applied later
        //   - and therefore execute earlier
        //
        // To ensure fallback entry alloc tracking runs at real function entry (after prologue) and
        // before other inserted hooks, we PREPEND these InsertPoints.
        let mut fallback_entry_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut fallback_return_points: Vec<InsertPoint<'tcx>> = Vec::new();

        for (local, size) in fallback_locals.iter().copied() {
            if local == RETURN_PLACE {
                continue;
            }
        
            // Only track stack slots that are actually address-taken (unless user forces all).
            if !track_all_stack_allocs && !interesting_stack_locals.contains(&local) {
                continue;
            }
        
            // Never record pointer-typed locals as allocations.
            let ty = body.local_decls[local].ty;
            if self.is_thin_ptr_ty(tcx, ty) {
                continue;
            }
            
            fallback_entry_points.push(InsertPoint {
                bb: START_BLOCK,
                stmt_idx: entry_insert_at,
                // Insert after rustc's StorageLive prologue statements.
                insert_before: false,
                source_info: entry_source_info,
                place: Place::from(local),
                kind: InstrKind::StackAlloc { local, live: true, size },
            });

            for (ret_bb, ret_source_info, ret_stmt_idx) in return_sites.iter().copied() {
                fallback_return_points.push(InsertPoint {
                    bb: ret_bb,
                    stmt_idx: ret_stmt_idx,
                    insert_before: false,
                    source_info: ret_source_info,
                    place: Place::from(local),
                    kind: InstrKind::StackAlloc { local, live: false, size },
                });
            }
        }

        // Prepend entry fallback points so they are applied last (and execute first) during insertion.
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
            InstrKind::StackAlloc { .. } => hooks.def_id_alloc,
            InstrKind::HeapAlloc { .. } => hooks.def_id_alloc,
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

                // Split the current block at stmt_idx.
                let mut tail_stmts: Vec<Statement<'tcx>> = Vec::new();
                {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let split_at = stmt_idx.min(bd.statements.len());
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

                // addr_local = expose_provenance(dst_local)
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let addr_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(addr_local),
                        Rvalue::Cast(
                            CastKind::PointerExposeProvenance,
                            Operand::Copy(Place::from(dst_local)),
                            tcx.types.usize,
                        ),
                    ))),
                );

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
                take_bd.statements.push(addr_stmt);
                take_bd.terminator = Some(take_term);

                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::RetPush { callee_id, ptr_local } = creation_kind {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RetPush");

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let addr_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(addr_local),
                        Rvalue::Cast(
                            CastKind::PointerExposeProvenance,
                            Operand::Copy(Place::from(ptr_local)),
                            tcx.types.usize,
                        ),
                    ))),
                );

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
                bd.statements.push(addr_stmt);
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

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let addr_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(addr_local),
                        Rvalue::Cast(
                            CastKind::PointerExposeProvenance,
                            Operand::Copy(Place::from(ptr_local)),
                            tcx.types.usize,
                        ),
                    ))),
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
                    bd.statements.push(addr_stmt);
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
                InstrKind::HeapAlloc { ptr_local, .. } => {
                    let s2 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(Place::from(ptr_local)),
                                tcx.types.usize,
                            ),
                        ))),
                    );
                    (None, s2)
                }
                _ => {
                    let s2 = Statement::new(
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
                    (None, s2)
                }
            };

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc { ptr_local, live, .. } => Some((*ptr_local, *live)),
                _ => None,
            };

            let arg_addr = Operand::Copy(Place::from(addr_local));

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead { ptr_local, size } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let arg_size0 = self.const_usize(tcx, source_info.span, size);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size0, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackAlloc { size, live, .. } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let arg_size = self.const_usize(tcx, source_info.span, size);
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

                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: size_op.clone(), span: source_info.span },
                        Spanned { node: arg_live, span: source_info.span },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrWrite { ptr_local, size } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op: Operand<'tcx> = if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                    let arg_size0 = self.const_usize(tcx, source_info.span, size);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned { node: tag_op, span: source_info.span },
                        Spanned { node: arg_addr, span: source_info.span },
                        Spanned { node: arg_size0, span: source_info.span },
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

        self.insert_instrumentation(
            tcx,
            body,
            scan.insert_points,
            &tag_local_for_ptr_local,
            hooks,
        );

        // Avoid reading uninitialized tag locals in callees: default them to 0 ("untagged").
        // IMPORTANT: do this AFTER insert_instrumentation so we don't invalidate `stmt_idx`
        // computed by scan_body for START_BLOCK (bb0).
        let mut arg_ptr_locals: HashSet<Local> = HashSet::new();
        for arg_local in body.args_iter() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_thin_ptr_ty(tcx, arg_ty) {
                arg_ptr_locals.insert(arg_local);
            }
        }

        self.init_tag_locals_to_zero(tcx, body, &tag_local_for_ptr_local, &arg_ptr_locals);
    }
}
