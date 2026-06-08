//! Scans non-call terminators that need pointer boundary hooks.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn scan_drop_terminator<'tcx>(
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
            ptr_locals_needing_tag.insert(place.local);
            tagged_ptr_locals.insert(place.local);
            let suppress_protector =
                !self.tb_call_arg_protector_supported_for_ty(tcx, body, dropped_ty);
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
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        from_shadow: !place.projection.is_empty(),
                        flags: self.call_arg_push_flags(false, suppress_protector),
                    },
                });
            }
            return;
        }

        if self.ty_contains_direct_ref_fields(tcx, dropped_ty) {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place,
                kind: InstrKind::CallArgValidate { local: place.local },
            });
        }

        if matches!(place.projection.first(), Some(ProjectionElem::Deref))
            && self.is_pointer_ty(body.local_decls[place.local].ty)
        {
            ptr_locals_needing_tag.insert(place.local);
            tagged_ptr_locals.insert(place.local);
            let suppress_protector = !self.tb_call_arg_protector_supported_for_ty(
                tcx,
                body,
                body.local_decls[place.local].ty,
            );
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
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        // `Drop(*ptr)` drops a value behind a pointer, not a pointer field.
                        // Export the pointer's borrow family; ptr shadow may not exist here.
                        from_shadow: false,
                        flags: self.call_arg_push_flags(false, suppress_protector),
                    },
                });
            }
            return;
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
                    parent_mode: ParentSelectionMode::SlotFamily,
                    from_shadow: false,
                    flags: 0,
                },
            });
        }
    }
}
