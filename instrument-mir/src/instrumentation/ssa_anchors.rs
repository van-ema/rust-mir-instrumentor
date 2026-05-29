use super::*;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) enum SsaAnchorSource {
    Tag,
    RefAncestor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::instrumentation) struct SsaAnchorState {
    pub(in crate::instrumentation) local: Local,
    pub(in crate::instrumentation) deps: Vec<Local>,
    pub(in crate::instrumentation) source: SsaAnchorSource,
}

pub(in crate::instrumentation) type SsaAnchorMap = HashMap<String, SsaAnchorState>;
pub(in crate::instrumentation) type ReborrowAnchorSpecMap = HashMap<String, Vec<Local>>;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn normalized_ptr_copy_anchor_key<'tcx>(
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

    pub(in crate::instrumentation) fn invalidate_ssa_anchors_for_local(
        &self,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        local: Local,
    ) {
        ssa_anchor_for_expr.retain(|_, state| !state.deps.contains(&local));
    }

    pub(in crate::instrumentation) fn rebind_ssa_anchors_for_copy(
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
    pub(in crate::instrumentation) fn reusable_ssa_anchor_for_expr<'tcx>(
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
    pub(in crate::instrumentation) fn reusable_ssa_anchor_for_ref_source_expr<'tcx>(
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
    pub(in crate::instrumentation) fn allow_ssa_anchor_reuse_for_ref_source_place<'tcx>(
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
    pub(in crate::instrumentation) fn invalidate_ssa_anchors_for_call<'tcx>(
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

    pub(in crate::instrumentation) fn meet_ssa_anchor_maps<'a>(
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
    pub(in crate::instrumentation) fn analyze_ssa_anchor_entry_maps<'tcx>(
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

    pub(in crate::instrumentation) fn normalized_ptr_expr_key_for_rvalue<'tcx>(
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

    pub(in crate::instrumentation) fn normalized_ptr_expr_key_for_ref_source_place<'tcx>(
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

    pub(in crate::instrumentation) fn projected_reborrow_anchor_dep_ok<'tcx>(
        &self,
        ty: Ty<'tcx>,
    ) -> bool {
        !matches!(ty.kind(), TyKind::RawPtr(..))
    }

    pub(in crate::instrumentation) fn projected_reborrow_anchor_eligible<'tcx>(
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

    pub(in crate::instrumentation) fn maybe_projected_reborrow_anchor_key<'tcx>(
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

    pub(in crate::instrumentation) fn projected_reborrow_anchor_allowed_for_src_place<'tcx>(
        &self,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> bool {
        matches!(
            self.parent_selection_mode_for_src_place(body, src_place),
            ParentSelectionMode::PointeeFamily
        )
    }

    pub(in crate::instrumentation) fn normalized_ptr_rvalue_key<'tcx>(
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

    pub(in crate::instrumentation) fn normalized_ptr_operand_key<'tcx>(
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

    pub(in crate::instrumentation) fn normalized_ptr_place_key<'tcx>(
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

    pub(in crate::instrumentation) fn normalized_ptr_local_key<'tcx>(
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
}
