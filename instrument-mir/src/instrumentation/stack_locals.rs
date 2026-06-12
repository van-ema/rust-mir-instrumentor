//! Finds stack locals and temporary refs that need instrumentation state.

use super::*;

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

#[derive(Copy, Clone, Debug, Default)]
pub(in crate::instrumentation) struct LocalRefUseStats {
    defs: usize,
    uses: usize,
}

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn compute_local_ref_use_stats<'tcx>(
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

    pub(in crate::instrumentation) fn local_has_observable_use_excluding_call_dest<'tcx>(
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

    pub(in crate::instrumentation) fn compute_summary_elidable_shared_call_ref_locals<'tcx>(
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

    pub(in crate::instrumentation) fn noescape_reborrow_call_temp_local<'tcx>(
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
                if let StatementKind::Assign(box (lhs, Rvalue::Ref(_, borrow_kind, src_place))) =
                    &stmt.kind
                {
                    let same_family_helper_reborrow =
                        matches!(borrow_kind, BorrowKind::Shared | BorrowKind::Mut { .. });
                    if lhs.as_local() == Some(local) && same_family_helper_reborrow {
                        def_src_place = Some(*src_place);
                    }
                }
            }
        }
        let src_place = def_src_place?;
        let same_family_ref_reborrow_src =
            matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
                && matches!(body.local_decls[src_place.local].ty.kind(), TyKind::Ref(..));
        let whole_place_slot_family_src =
            self.is_whole_place_slot_family_source(tcx, body, src_place);
        if !same_family_ref_reborrow_src && !whole_place_slot_family_src {
            return None;
        }

        let Some((callee_did, _)) = self.direct_callee(tcx, body, block_data, func) else {
            return None;
        };
        if self.is_instrumented_callee(tcx, callee_did)
            && (same_family_ref_reborrow_src || whole_place_slot_family_src)
        {
            // This caller-side `&*base`/same-family temp is administrative: the call-boundary
            // import creates the callee-side child that actually models any real escape or later
            // return. Marking the caller temp as escaped via `PtrUse` keeps an artificial
            // same-lineage sibling alive past the call and diverges from TB/Miri's transient
            // receiver reborrow behavior for helper chains like `iter_mut`, `next`, `len`,
            // `capacity`, and `as_ptr`.
            return Some(local);
        }
        let summary_allows =
            unsafe_dataflow::summary_for_def_id(tcx, callee_did).and_then(|summary| {
                summary
                    .ptr_args()
                    .iter()
                    .find(|entry| entry.arg_index() == arg_index)
                    .map(|arg_summary| {
                        !arg_summary.reaches_direct_sink()
                            && !arg_summary.escapes_to_unknown_boundary()
                            && !arg_summary.forwarded_to_return()
                    })
            });
        if !matches!(summary_allows, Some(true)) {
            return None;
        }

        Some(local)
    }

    pub(in crate::instrumentation) fn compute_call_only_reborrow_forward_sources<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local_ref_use_stats: &HashMap<Local, LocalRefUseStats>,
    ) -> HashMap<Local, Local> {
        let mut def_src_by_local: HashMap<Local, Local> = HashMap::new();

        for block_data in body.basic_blocks.iter() {
            for stmt in &block_data.statements {
                let StatementKind::Assign(box (lhs, Rvalue::Ref(_, borrow_kind, src_place))) =
                    &stmt.kind
                else {
                    continue;
                };
                if !matches!(borrow_kind, BorrowKind::Shared | BorrowKind::Mut { .. }) {
                    continue;
                }
                let Some(local) = lhs.as_local() else {
                    continue;
                };
                let local_ty = body.local_decls[local].ty;
                if !matches!(local_ty.kind(), TyKind::Ref(..)) {
                    continue;
                }
                if !matches!(src_place.projection.as_slice(), [ProjectionElem::Deref]) {
                    continue;
                }
                if !matches!(body.local_decls[src_place.local].ty.kind(), TyKind::Ref(..)) {
                    continue;
                }
                if local_ref_use_stats
                    .get(&local)
                    .is_none_or(|stat| stat.defs != 1 || stat.uses != 1)
                {
                    continue;
                }
                def_src_by_local.insert(local, src_place.local);
            }
        }

        let mut forward_sources = HashMap::new();
        for block_data in body.basic_blocks.iter() {
            let Some(term) = &block_data.terminator else {
                continue;
            };
            let TerminatorKind::Call { func, args, .. } = &term.kind else {
                continue;
            };
            let Some((callee_did, _)) = self.direct_callee(tcx, body, block_data, func) else {
                continue;
            };
            if !self.is_instrumented_callee(tcx, callee_did) {
                continue;
            }
            for arg in args.iter() {
                let Some(place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                if !place.projection.is_empty() {
                    continue;
                }
                if let Some(src_local) = def_src_by_local.get(&place.local).copied() {
                    forward_sources.insert(place.local, src_local);
                }
            }
        }

        forward_sources
    }

    pub(in crate::instrumentation) fn compute_interesting_stack_locals<'tcx>(
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

    pub(in crate::instrumentation) fn track_all_stack_allocs_flag(&self) -> bool {
        false
    }

    pub(in crate::instrumentation) fn entry_insert_after_prologue<'tcx>(
        &self,
        body: &Body<'tcx>,
    ) -> usize {
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
}
