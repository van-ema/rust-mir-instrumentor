// src/main.rs
#![allow(unused)]
#![feature(rustc_private)]
#![feature(box_patterns)]
extern crate rustc_abi;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_mir_transform;
use std::collections::{HashMap, HashSet};
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_target;

use rustc_abi::ExternAbi;
use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_hir::def_id::{DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use rustc_interface::util::rustc_path;
use rustc_interface::Config;
use rustc_middle::mir::interpret::{AllocId, Scalar};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::{self, print, ParamEnv, PseudoCanonicalInput, Ty, TyCtxt, TypingEnv};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;
use rustc_span::{source_map::Spanned, Span};

use rustc_hir::{Mutability, Safety};
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::pretty::write_mir_fn;
use rustc_middle::ty::TyKind;
use rustc_span::symbol::Symbol;
use std::num::NonZeroU64;
use std::sync::Mutex;

use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::sync::OnceLock;

static MIR_OUT_BEFORE: OnceLock<String> = OnceLock::new();
static MIR_OUT_AFTER: OnceLock<String> = OnceLock::new();

fn prefixed_path(base: &str, prefix: &str) -> String {
    use std::path::{Path, PathBuf};

    let p = Path::new(base);
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let file_name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mir.txt".to_string());

    let mut out: PathBuf = parent.to_path_buf();
    out.push(format!("{}{}", prefix, file_name));
    out.to_string_lossy().to_string()
}

struct MyOptimizationPass;

#[derive(Copy, Clone, Debug)]
enum InstrKind<'tcx> {
    Ref { bk: BorrowKind, src: Place<'tcx> },
    Raw { is_mut: bool, src: Place<'tcx> },
    /// Stack allocation lifetime event for a MIR local.
    StackAlloc { local: Local, live: bool, size: usize },
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
    /// Caller-side tag push for pointer arguments to a direct call.
    CallArgPush { callee_id: u64, arg_index: u64, ptr_local: Local },
    /// Callee-side retagging of pointer arguments from the runtime side-channel.
    ArgRetag { callee_id: u64, arg_index: u64, ptr_local: Local },
}

#[derive(Clone, Debug)]
struct InsertPoint<'tcx> {
    bb: BasicBlock,
    stmt_idx: usize,
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
}

impl MyOptimizationPass {
    fn is_ptr_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
    }

    fn callee_id_u64(&self, def_id: DefId) -> u64 {
        ((def_id.krate.as_u32() as u64) << 32) | (def_id.index.as_u32() as u64)
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

    fn scan_body<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> ScanResult<'tcx> {
        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();

        // Filter: only track stack locals that are likely to matter for unsafe behavior.
        // Heuristic: instrument StorageLive/StorageDead only for locals whose address is taken
        // to create a reference or raw pointer (i.e., appear as the base local in `Rvalue::Ref`
        // or `Rvalue::RawPtr`). This dramatically reduces noise from compiler-introduced temporaries.
        let mut interesting_stack_locals: HashSet<Local> = HashSet::new();
        for (_bb, block_data) in body.basic_blocks.iter_enumerated() {
            for stmt in block_data.statements.iter() {
                if let StatementKind::Assign(box (_dst, rv)) = &stmt.kind {
                    match rv {
                        // Direct address taking.
                        Rvalue::Ref(_, _bk, src_place) => {
                            if src_place.local != RETURN_PLACE {
                                interesting_stack_locals.insert(src_place.local);
                            }
                        }
                        Rvalue::RawPtr(_mutbl, src_place) => {
                            if src_place.local != RETURN_PLACE {
                                interesting_stack_locals.insert(src_place.local);
                            }
                        }

                        // Common deref-related temporary; treat the source local as interesting.
                        Rvalue::CopyForDeref(p) => {
                            if p.local != RETURN_PLACE {
                                interesting_stack_locals.insert(p.local);
                            }
                        }

                        // Pointer-related casts/coercions that often show up in optimized MIR.
                        // If the operand comes from a local place, mark that local as interesting.
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute
                            | CastKind::PointerExposeProvenance,
                            op,
                            _to_ty,
                        ) => {
                            if let Some(src_place) = self.place_from_operand(op) {
                                if src_place.local != RETURN_PLACE {
                                    interesting_stack_locals.insert(src_place.local);
                                }
                            }
                        }

                        _ => {}
                    }
                }
            }
        }

        // Two-tier strategy: by default we filter stack alloc events to reduce noise.
        // Set `RZ_STACK_ALLOCS=all` (or 1/true) to instrument StorageLive/StorageDead for all locals.
        let track_all_stack_allocs = std::env::var("RZ_STACK_ALLOCS")
            .map(|v| v == "all" || v == "ALL" || v == "1" || v == "true" || v == "TRUE")
            .unwrap_or(false);

        let entry_bb = START_BLOCK;
        let entry_bd = &body.basic_blocks[entry_bb];
        let mut entry_insert_at = 0usize;
        while entry_insert_at < entry_bd.statements.len() {
            match entry_bd.statements[entry_insert_at].kind {
                StatementKind::StorageLive(_) => entry_insert_at += 1,
                _ => break,
            }
        }

        let entry_source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };
        let callee_id = self.callee_id_u64(body.source.def_id());
        
        for (arg_index, arg_local) in body.args_iter().enumerate() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_ptr_ty(arg_ty) {
                // Callee-side retagging: use the caller-pushed tag as parent at entry.
                ptr_locals_needing_tag.insert(arg_local);
                insert_points.push(InsertPoint {
                    bb: entry_bb,
                    stmt_idx: entry_insert_at,
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

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                // Stack allocation lifetime: StorageLive/StorageDead.
                match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        if local != RETURN_PLACE
                            && (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                        {
                            let live = matches!(stmt.kind, StatementKind::StorageLive(_));
                            let ty = body.local_decls[local].ty;
                            let size = self.layout_size_bytes(tcx, ty);

                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc { local, live, size },
                            });
                        }
                    }
                    _ => {}
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
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            source_info: stmt.source_info,
                            place: Place::from(ptr_local),
                            kind: InstrKind::PtrWrite { ptr_local, size: 0 },
                        });
                    }
                }

                // Tag propagation across pointer-to-pointer casts and plain copies/moves of pointer locals.
                if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
                    if let Some(dst_local) = dst_place.as_local() {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if self.is_ptr_ty(dst_ty) {
                            let src_local_opt: Option<Local> = match rvalue {
                                // Plain copy/move of a pointer local.
                                Rvalue::Use(op) => self
                                    .place_from_operand(op)
                                    .and_then(|p| p.as_local()),

                                // Common form used around deref-based ops.
                                Rvalue::CopyForDeref(p) => p.as_local(),

                                // Pointer-to-pointer and pointer coercions/transmutes.
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
                                if self.is_ptr_ty(src_ty) {
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);

                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::TagProp { dst: dst_local, src: src_local },
                                    });
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
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Raw { is_mut, src: src_place.clone() },
                    });
                }
            }

            // Calls: classify volatile_{store,load} and coarse PtrUse for pointer args.
            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call { func, args, .. } = &term.kind {
                    let mut is_volatile_store = false;
                    let mut is_volatile_load = false;
                    let mut callee_id_opt: Option<u64> = None;

                    if let TyKind::FnDef(callee_def_id, _) = func.ty(body, tcx).kind() {
                        let path = tcx.def_path_str(*callee_def_id);
                        is_volatile_store = path.contains("intrinsics::volatile_store");
                        is_volatile_load = path.contains("intrinsics::volatile_load");
                        callee_id_opt = Some(self.callee_id_u64(*callee_def_id));
                    }

                    let mut classified_write_ptr_local: Option<Local> = None;
                    let mut classified_read_ptr_local: Option<Local> = None;

                    if is_volatile_store {
                        if let Some(first) = args.get(0) {
                            if let Some(p0) = self.place_from_operand(&first.node) {
                                classified_write_ptr_local = Some(p0.local);
                                let mut size = 0usize;
                                let ty0 = body.local_decls[p0.local].ty;
                                if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                    size = self.layout_size_bytes(tcx, *pointee_ty);
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    source_info: term.source_info,
                                    place: Place::from(p0.local),
                                    kind: InstrKind::PtrWrite { ptr_local: p0.local, size },
                                });
                            }
                        }
                    }

                    if is_volatile_load {
                        if let Some(first) = args.get(0) {
                            if let Some(p0) = self.place_from_operand(&first.node) {
                                classified_read_ptr_local = Some(p0.local);
                                let mut size = 0usize;
                                let ty0 = body.local_decls[p0.local].ty;
                                if let TyKind::RawPtr(pointee_ty, _mutbl) = ty0.kind() {
                                    size = self.layout_size_bytes(tcx, *pointee_ty);
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    source_info: term.source_info,
                                    place: Place::from(p0.local),
                                    kind: InstrKind::PtrRead { ptr_local: p0.local, size },
                                });
                            }
                        }
                    }

                    for (arg_index, a) in args.iter().enumerate() {
                        if let Some(p) = self.place_from_operand(&a.node) {
                            let ty = body.local_decls[p.local].ty;
                            if self.is_ptr_ty(ty) {
                                if let Some(callee_id) = callee_id_opt {
                                    // Caller-side push: this tag is the parent for callee ArgRetag.
                                    ptr_locals_needing_tag.insert(p.local);
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx: block_data.statements.len(),
                                        source_info: term.source_info,
                                        place: Place::from(p.local),
                                        kind: InstrKind::CallArgPush {
                                            callee_id,
                                            arg_index: arg_index as u64,
                                            ptr_local: p.local,
                                        },
                                    });
                                }
                                if classified_write_ptr_local == Some(p.local)
                                    || classified_read_ptr_local == Some(p.local)
                                {
                                    continue;
                                }
                                // IMPORTANT: Ensure pointer-typed call arguments get an associated tag local.
                                // Without this, PtrUse events at call boundaries would often observe tag=0,
                                // because the pointer local was never marked as needing a tag. We treat
                                // passing a pointer into a function as a coarse "use"/escape boundary.
                                ptr_locals_needing_tag.insert(p.local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    source_info: term.source_info,
                                    place: Place::from(p.local),
                                    kind: InstrKind::PtrUse { ptr_local: p.local },
                                });
                            }
                        }
                    }
                }
            }
        }

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
            InstrKind::StackAlloc { .. } => hooks.def_id_alloc,
            InstrKind::PtrWrite { .. } => hooks.def_id_write,
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::TagProp { .. } => hooks.def_id_use, // unreachable in practice
            InstrKind::CallArgPush { .. } => hooks.def_id_push_call_arg_tag,
            InstrKind::ArgRetag { .. } => hooks.def_id_take_call_arg_tag,
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

            if let InstrKind::TagProp { dst, src } = creation_kind {
                println!(
                    "[instrument-mir] TAG PROPAGATION: dst_local={:?} src_local={:?}",
                    dst,
                    src
                );
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

            let call_term = Terminator {
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

            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

                let split_at = if stmt_idx >= bd.statements.len() {
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
    /*
    fn print_runtime_items<'tcx>(&self, tcx: TyCtxt<'tcx>) {
        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                println!("Items in runtime crate:");
                let items = tcx.hir_crate_items(());
                // Use `free_items` to iterate over non-associated items
                for item_id in items.free_items() {
                    let def_id = item_id.owner_id.def_id;
                    if let Some(name) = tcx.opt_item_name(def_id) {
                        println!(" - Item: {}", name);
                    } else {
                        println!(" - Unnamed item: {:?}", def_id);
                    }
                    // Print additional debugging information about the item
                    let item_kind = tcx.def_kind(def_id);
                    println!("   - DefKind: {:?}", item_kind);
                    let span = tcx.def_span(def_id);
                    println!("   - Span: {:?}", span);
                }
            }
        }
    }
    */

    fn find_def_id_by_name<'tcx>(&self, tcx: TyCtxt<'tcx>, target_name: &str) -> Option<DefId> {
        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                println!("Searching for '{}' in runtime crate:", target_name);
                let items = tcx.exported_non_generic_symbols(cnum);
                println!("Found {} items", items.len());
                for (symbol, _) in items {
                    match symbol {
                        ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                            if let Some(name) = tcx.opt_item_name(*def_id) {
                                println!(" - Checking item: {}", name);
                                if name.as_str() == target_name {
                                    println!(" - Match found for '{}'", target_name);
                                    return Some(*def_id);
                                }
                            } else {
                                println!(" - Unnamed item: {:?}", def_id);
                            }
                        }
                        ExportedSymbol::NoDefId(symbol_name) => {
                            println!(" - Symbol without DefId: {:?}", symbol_name);
                        }
                        ExportedSymbol::DropGlue(ty) => {
                            println!(" - DropGlue for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlueCtorShim(ty) => {
                            println!(" - AsyncDropGlueCtorShim for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlue(def_id, ty) => {
                            println!(" - AsyncDropGlue for DefId: {:?}, type: {:?}", def_id, ty);
                        }
                        ExportedSymbol::ThreadLocalShim(def_id) => {
                            println!(" - ThreadLocalShim for DefId: {:?}", def_id);
                        }
                        _ => {
                            println!(" - Unhandled ExportedSymbol variant");
                        }
                    }
                }
            }
        }
        println!("No match found for '{}'", target_name);
        None
    }

    fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
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

        println!("Loaded crates:");
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum);
            println!(" - {:?} (cnum: {:?})", name, cnum);
        }

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

        let hooks = Hooks {
            def_id_ref,
            def_id_raw,
            def_id_alloc,
            def_id_write,
            def_id_read,
            def_id_use,
            def_id_push_call_arg_tag,
            def_id_take_call_arg_tag,
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
            if self.is_ptr_ty(arg_ty) {
                arg_ptr_locals.insert(arg_local);
            }
        }

        self.init_tag_locals_to_zero(tcx, body, &tag_local_for_ptr_local, &arg_ptr_locals);
    }
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        // Write MIR before running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_BEFORE.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let def_path = tcx.def_path_str(body.source.def_id());

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let mut writer = BufWriter::new(file);

            writeln!(&mut writer, "\n\n// ===== MIR BEFORE: {} =====", def_path).unwrap();

            write_mir_fn(
                tcx,
                &body,
                &mut extra,
                &mut writer,
                rustc_middle::mir::pretty::PrettyPrintMirOptions::from_cli(tcx),
            )
            .unwrap();
            writer.flush().unwrap();
        }

        let optimization_pass = MyOptimizationPass;
        optimization_pass.run_pass(tcx, &mut body);

        // Write MIR after running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_AFTER.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let def_path = tcx.def_path_str(body.source.def_id());

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let mut writer = BufWriter::new(file);

            writeln!(&mut writer, "\n\n// ===== MIR AFTER: {} =====", def_path).unwrap();

            write_mir_fn(
                tcx,
                &body,
                &mut extra,
                &mut writer,
                rustc_middle::mir::pretty::PrettyPrintMirOptions::from_cli(tcx),
            )
            .unwrap();
            writer.flush().unwrap();
        }

        tcx.arena.alloc(body)
    };

struct CompilerCallbacks;

impl rustc_driver::Callbacks for CompilerCallbacks {
    fn config(&mut self, _config: &mut Config) {
        _config.override_queries = Some(|_session, queries| {
            queries.optimized_mir = CUSTOM_OPT_MIR;
        });
    }
}

fn main() {
    let mut callbacks = CompilerCallbacks {};

    let handler = EarlyDiagCtxt::new(ErrorOutputType::HumanReadable {
        kind: HumanReadableErrorType::Default,
        color_config: ColorConfig::Auto,
    });
    rustc_driver::init_rustc_env_logger(&handler);
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let mut args: Vec<String> = std::env::args().collect();

        let mut mir_out: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--mir-out=") {
                mir_out = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        let mut runtime_path: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--runtime-path=") {
                runtime_path = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        if let Some(p) = mir_out {
            let before = prefixed_path(&p, "before.");
            let after = prefixed_path(&p, "after.");
            MIR_OUT_BEFORE.set(before).unwrap();
            MIR_OUT_AFTER.set(after).unwrap();
        }

        // Cargo probes the compiler with `-vV` (verbose version) before building.
        // That invocation won't carry our custom flags, so we must not require them.
        let is_version_probe = args
            .iter()
            .any(|a| a == "-vV" || a == "-V" || a == "--version");

        if let Some(runtime_path) = runtime_path {
            args.push("-Zunstable-options".to_string());
            args.push(format!("-L{}", runtime_path));
            args.push(format!(
                "--extern=force:runtime={}/libruntime.rlib",
                runtime_path
            ));
        } else if !is_version_probe {
            panic!("missing --runtime-path argument (pass it via `cargo instrument-mir --runtime-path=...`)");
        }
        // args.push("-Zdump-mir=main".to_string());
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}
