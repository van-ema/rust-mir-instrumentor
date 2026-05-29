//! Recovers pointer sources and chooses parent tags for new pointer views.

use super::*;

impl MyOptimizationPass {
    /// Best-effort: recover a pointer-typed source local that feeds `dst_local` in the same block.
    ///
    /// This follows trivial forwarding/casts and ref/raw creations:
    /// - `Use`, `CopyForDeref`
    /// - pointer casts/coercions/transmute
    /// - `Ref` / `RawPtr` assignments (returns their source local when pointer-typed)
    ///
    /// We use it to avoid falling back to `parent=0` for ref/raw creations when the immediate
    /// source local is a temporary projection local instead of the real pointer carrier.
    pub(in crate::instrumentation) fn backtrack_pointer_source_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_global_pointer_value_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_global_exposed_provenance_source_local<'tcx>(
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

    pub(in crate::instrumentation) fn rhs_carries_boundary_recovered_ptr<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_global_single_pointer_carrier_local<'tcx>(
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
    pub(in crate::instrumentation) fn backtrack_single_pointer_carrier_local<'tcx>(
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
    pub(in crate::instrumentation) fn recover_parent_source_local_for_place<'tcx>(
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

    pub(in crate::instrumentation) fn recover_pointer_source_local_for_projected_place<'tcx>(
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

    pub(in crate::instrumentation) fn receiver_family_base_local_for_place<'tcx>(
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

    pub(in crate::instrumentation) fn receiver_family_parent_operand_for_place<'tcx>(
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
        if let Some(op) = self
            .slot_family_parent_operand_for_local(base_local, reborrow_anchor_local_for_stack_local)
        {
            return Some(op);
        }

        let _ = (tcx, source_info);
        None
    }

    pub(in crate::instrumentation) fn exact_or_ref_ancestor_parent_operand_for_local<'tcx>(
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

    pub(in crate::instrumentation) fn slot_family_parent_operand_for_local<'tcx>(
        &self,
        local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<Operand<'tcx>> {
        reborrow_anchor_local_for_stack_local
            .get(&local)
            .copied()
            .map(|anchor_local| Operand::Copy(Place::from(anchor_local)))
    }

    pub(in crate::instrumentation) fn is_projected_carrier_raw_field_load<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
    ) -> bool {
        is_raw_creation
            && !src_place.projection.is_empty()
            && !self.is_pointer_ty(body.local_decls[src_place.local].ty)
    }

    pub(in crate::instrumentation) fn is_projected_raw_field_load(
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

    pub(in crate::instrumentation) fn projected_slot_family_parent_operand_for_src_place<'tcx>(
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

    pub(in crate::instrumentation) fn projected_fast_path_pointee_parent_operand_for_src_place<
        'tcx,
    >(
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

    pub(in crate::instrumentation) fn candidate_parent_source_local_for_src_place<'tcx>(
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

    pub(in crate::instrumentation) fn pointee_family_parent_operand_for_src_place<'tcx>(
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

    pub(in crate::instrumentation) fn parent_selection_mode_for_src_place<'tcx>(
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

    pub(in crate::instrumentation) fn creation_parent_selection_mode_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        is_raw_creation: bool,
        use_projectionless_anchor: bool,
    ) -> ParentSelectionMode {
        if matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::ReceiverFamily
        ) {
            return ParentSelectionMode::ReceiverFamily;
        }

        if !is_raw_creation
            && src_place.projection.is_empty()
            && self.is_pointer_ty(body.local_decls[src_place.local].ty)
        {
            return ParentSelectionMode::SlotFamily;
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
    pub(in crate::instrumentation) fn parent_tag_operand_for_src_place<'tcx>(
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
        let _ = (
            projectionless_anchor_suppressed_locals,
            use_projectionless_anchor,
        );
        let src_local_ty = body.local_decls[src_place.local].ty;
        let projectionless_raw_direct_pointer_carrier = is_raw_creation
            && src_place.projection.is_empty()
            && !self.is_pointer_ty(src_local_ty)
            && self.ty_contains_direct_pointer_fields(tcx, body, src_local_ty);
        match mode {
            ParentSelectionMode::ReceiverFamily => self
                .receiver_family_parent_operand_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    reborrow_anchor_local_for_stack_local,
                )
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
                .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0)),
            ParentSelectionMode::SlotFamily => self
                .slot_family_parent_operand_for_local(
                    src_place.local,
                    reborrow_anchor_local_for_stack_local,
                )
                .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0)),
            ParentSelectionMode::PointeeFamily => self
                .projected_fast_path_pointee_parent_operand_for_src_place(
                    tcx,
                    body,
                    src_place,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    is_raw_creation,
                )
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
                .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0)),
        }
    }

    pub(in crate::instrumentation) fn materialize_projected_reborrow_parent_local<'tcx>(
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

    pub(in crate::instrumentation) fn materialize_projectionless_slot_anchor_parent_local<'tcx>(
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

    pub(in crate::instrumentation) fn materialize_boundary_recovered_source_tag_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        src_local: Local,
        src_tag_local: Local,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        extra_stmts: &mut Vec<Statement<'tcx>>,
    ) -> Local {
        if !matches!(
            body.local_decls[src_local].ty.kind(),
            TyKind::Ref(_, _, Mutability::Not)
        ) {
            return src_tag_local;
        }

        let (Some(export_parent_local), Some(recovered_local)) = (
            export_parent_local_for_ptr_local.get(&src_local).copied(),
            export_parent_is_recovered_local_for_ptr_local
                .get(&src_local)
                .copied(),
        ) else {
            return src_tag_local;
        };

        let recovered_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let not_recovered_u64_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let exact_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let export_part_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));
        let selected_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.u64, source_info.span));

        extra_stmts.extend([
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
                    Place::from(exact_part_local),
                    Rvalue::BinaryOp(
                        BinOp::Mul,
                        Box::new((
                            Operand::Copy(Place::from(not_recovered_u64_local)),
                            Operand::Copy(Place::from(src_tag_local)),
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
                            Operand::Copy(Place::from(exact_part_local)),
                            Operand::Copy(Place::from(export_part_local)),
                        )),
                    ),
                ))),
            ),
        ]);

        selected_local
    }

    pub(in crate::instrumentation) fn backtrack_unique_aggregate_anchor_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        dst_local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<Local> {
        let statements = &body.basic_blocks[bb].statements;
        let end = stmt_idx.min(statements.len());
        for stmt in statements[..end].iter().rev() {
            let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                continue;
            };
            if dst_place.as_local() != Some(dst_local) {
                continue;
            }
            let Rvalue::Aggregate(_, operands) = rvalue else {
                return None;
            };
            let mut unique_src_local: Option<Local> = None;
            let mut ambiguous = false;
            for operand in operands.iter() {
                let Some(src_place) = self.place_from_operand(operand) else {
                    continue;
                };
                if !src_place.projection.is_empty() {
                    continue;
                }
                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                if !self.is_pointer_ty(src_ty)
                    && !reborrow_anchor_local_for_stack_local.contains_key(&src_place.local)
                {
                    continue;
                }
                match unique_src_local {
                    Some(existing) if existing != src_place.local => {
                        ambiguous = true;
                        break;
                    }
                    Some(_) => {}
                    None => unique_src_local = Some(src_place.local),
                }
            }
            return if ambiguous { None } else { unique_src_local };
        }
        None
    }
}
