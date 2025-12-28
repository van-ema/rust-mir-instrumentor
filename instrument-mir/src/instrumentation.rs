use std::collections::{HashMap, HashSet};

use rustc_hir::def_id::DefId;
use rustc_hir::Mutability;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::Scalar;
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::{PseudoCanonicalInput, Ty, TyCtxt, TypingEnv};
use rustc_middle::ty::TyKind;
use rustc_span::{source_map::Spanned, Span};

pub(crate) struct MyOptimizationPass;

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
                        // Direct address taking.
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

                        // Common deref-related temporary; treat the source local as interesting.
                        Rvalue::CopyForDeref(p) => {
                            if p.local != RETURN_PLACE {
                                interesting.insert(p.local);
                            }
                        }

                        // Pointer-related casts/coercions that often show up in optimized MIR.
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
                                    interesting.insert(src_place.local);
                                }
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
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
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

    fn classify_call<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        func: &Operand<'tcx>,
    ) -> (bool, bool, Option<u64>) {
        let mut is_volatile_store = false;
        let mut is_volatile_load = false;
        let mut callee_id_opt: Option<u64> = None;

        if let TyKind::FnDef(callee_def_id, _) = func.ty(body, tcx).kind() {
            callee_id_opt = Some(self.callee_id_u64(*callee_def_id));

            // Option B: classify only true Rust intrinsics (no wrapper/libc lists).
            // Depending on toolchain/optimization, MIR may refer to intrinsics via
            // `core::intrinsics::*` or `std::intrinsics::*`.
            let path = tcx.def_path_str(*callee_def_id);
            if path.starts_with("core::intrinsics::") || path.starts_with("std::intrinsics::") {
                // Prefer the intrinsic item name rather than the full path.
                // `item_name` returns a `Symbol`; avoid borrowing `&str` from a temporary.
                let name_sym: rustc_span::symbol::Symbol = tcx.item_name(*callee_def_id);
                match name_sym.as_str() {
                    "volatile_store" => is_volatile_store = true,
                    "volatile_load" => is_volatile_load = true,
                    _ => {}
                }
            }
        }
        (is_volatile_store, is_volatile_load, callee_id_opt)
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
        let (is_volatile_store, is_volatile_load, callee_id_opt) = self.classify_call(tcx, body, func);

        // Tag propagation through pointer-returning calls.
        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.is_thin_ptr_ty(tcx, dst_ty) {
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
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::TagProp { dst: dst_local, src: src_local },
                    });
                }
            }
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
                        insert_before: false,
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
                        insert_before: false,
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
                if self.is_thin_ptr_ty(tcx, ty) {
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
                    if classified_write_ptr_local == Some(p.local)
                        || classified_read_ptr_local == Some(p.local)
                    {
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
            }
        }

        // Caller-side return-tag recovery: if the call returns a thin pointer into a local, take the tag.
        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.is_thin_ptr_ty(tcx, dst_ty) {
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
            InstrKind::StackAlloc { .. } => hooks.def_id_alloc,
            InstrKind::PtrWrite { .. } => hooks.def_id_write,
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::TagProp { .. } => hooks.def_id_use, // unreachable in practice
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

            let insert_before = ip.insert_before
                || matches!(
                    creation_kind,
                    InstrKind::PtrRead { .. } | InstrKind::PtrWrite { .. }
                );
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
