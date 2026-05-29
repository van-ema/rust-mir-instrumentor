//! Backtracks MIR assignments to recover pointer sources and pointees.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn fn_def_id_from_place<'tcx>(
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
    pub(in crate::instrumentation) fn fn_def_id_from_operand<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_fn_ptr_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        local: Local,
    ) -> Option<DefId> {
        fn local_from_operand<'tcx>(op: &Operand<'tcx>) -> Option<Local> {
            match op {
                Operand::Copy(place) | Operand::Move(place) => place.as_local(),
                Operand::Constant(_) => None,
            }
        }

        fn inner<'tcx>(
            pass: &MyOptimizationPass,
            tcx: TyCtxt<'tcx>,
            body: &Body<'tcx>,
            block_data: &BasicBlockData<'tcx>,
            local: Local,
            visited: &mut HashSet<Local>,
        ) -> Option<DefId> {
            if !visited.insert(local) {
                return None;
            }

            // Best-effort recovery for calls like:
            //   _f0 = safe as fn(&T, &mut T);
            //   _f1 = move _f0 as fn(*const T, *mut T) (Transmute);
            //   _0 = _f1(args...);
            for stmt in block_data.statements.iter().rev() {
                let StatementKind::Assign(box (dst, rvalue)) = &stmt.kind else {
                    continue;
                };
                if dst.as_local() != Some(local) {
                    continue;
                }
                match rvalue {
                    Rvalue::Use(op) | Rvalue::Cast(_, op, _) => {
                        if let Some(def_id) = pass.fn_def_id_from_operand(tcx, body, op) {
                            return Some(def_id);
                        }
                        if let Some(src_local) = local_from_operand(op) {
                            return inner(pass, tcx, body, block_data, src_local, visited);
                        }
                    }
                    Rvalue::CopyForDeref(place) => {
                        if let Some(def_id) = pass.fn_def_id_from_place(tcx, body, *place) {
                            return Some(def_id);
                        }
                        if let Some(src_local) = place.as_local() {
                            return inner(pass, tcx, body, block_data, src_local, visited);
                        }
                    }
                    _ => {}
                }
            }
            None
        }

        let mut visited = HashSet::new();
        inner(self, tcx, body, block_data, local, &mut visited)
    }
    pub(in crate::instrumentation) fn const_fn_def_id<'tcx>(
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
    pub(in crate::instrumentation) fn resolve_instance_def_id<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_unsize_base_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_aggregate_field_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_global_aggregate_field_local<'tcx>(
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
    pub(in crate::instrumentation) fn downcast_field_projection_index<'tcx>(
        &self,
        place: Place<'tcx>,
    ) -> Option<usize> {
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
    pub(in crate::instrumentation) fn resolve_ptr_local_for_call_place<'tcx>(
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
    pub(in crate::instrumentation) fn recover_projected_pointer_rhs_source_local<'tcx>(
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
    pub(in crate::instrumentation) fn recover_projected_pointer_rhs_source<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_deref_base_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_mut_ref_pointee_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_mut_ref_pointee_local_body<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_pointer_pointee_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_pointer_pointee_local_body<'tcx>(
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
}
