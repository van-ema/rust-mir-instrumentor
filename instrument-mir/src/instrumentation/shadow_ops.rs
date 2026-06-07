//! Emits and maintains pointer-shadow operations for MIR places and aggregates.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn shadow_store_uses_local_slot_store<'tcx>(
        &self,
        local_slot_shadow_store_locals: &HashSet<Local>,
        place: Place<'tcx>,
        src_local: Local,
    ) -> bool {
        place.as_local() == Some(src_local)
            && place.projection.is_empty()
            && local_slot_shadow_store_locals.contains(&src_local)
    }

    pub(in crate::instrumentation) fn aggregate_field_specs_for_kind<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        dst_ty: Ty<'tcx>,
        aggregate_kind: &AggregateKind<'tcx>,
        ops: &[Operand<'tcx>],
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
            (
                _,
                AggregateKind::Closure(..)
                | AggregateKind::Coroutine(..)
                | AggregateKind::CoroutineClosure(..),
            ) => Some((
                None,
                // Captured operands become fields in order, so their shadow moves
                // with the pointer value just like tuple and struct fields.
                ops.iter()
                    .enumerate()
                    .map(|(idx, op)| (idx, op.ty(&body.local_decls, tcx)))
                    .collect(),
            )),
            _ => None,
        }
    }

    pub(in crate::instrumentation) fn emit_aggregate_shadow_ops<'tcx>(
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
            self.aggregate_field_specs_for_kind(tcx, body, dst_ty, aggregate_kind, ops)
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
}
