//! Scans MIR statements and schedules pointer instrumentation hooks.

use super::*;

impl MyOptimizationPass {
    fn ref_pointer_coercion_preserves_tag<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        rvalue: &Rvalue<'tcx>,
        dst_ty: Ty<'tcx>,
    ) -> bool {
        let Rvalue::Cast(CastKind::PointerCoercion(_, _), op, _) = rvalue else {
            return false;
        };
        let src_ty = op.ty(&body.local_decls, tcx);
        // Example: `&mut T` coerced to `&mut dyn Trait`.
        // The data pointer is unchanged, so the borrow tag is unchanged too.
        matches!(src_ty.kind(), TyKind::Ref(..)) && matches!(dst_ty.kind(), TyKind::Ref(..))
    }

    /// Ensure `ptr_local` has a tag by synthesizing a `RawRoot` before the current statement
    /// if it hasn't been tagged yet.
    ///
    /// For wide pointers (`&[T]`, `&str`, `dyn Trait`), RawRoot lowering will first extract the
    /// thin data pointer (dropping metadata) and root-tag that address.
    pub(in crate::instrumentation) fn ensure_raw_root_before<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        ptr_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
    ) {
        if tagged_ptr_locals.contains(&ptr_local)
            || ptr_locals_with_tag_sources.contains(&ptr_local)
        {
            return;
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_pointer_ty(ptr_ty) {
            return;
        }

        let is_mut = self.ptr_is_mut(ptr_ty);
        let is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
        let projected_src =
            self.recover_projected_pointer_rhs_source(tcx, body, bb, stmt_idx, ptr_local);
        if let Some((def_bb, def_stmt_idx, src_place)) = projected_src {
            tagged_ptr_locals.insert(ptr_local);
            ptr_locals_needing_tag.insert(ptr_local);
            insert_points.push(InsertPoint {
                bb: def_bb,
                stmt_idx: def_stmt_idx,
                insert_before: false,
                source_info,
                place: src_place,
                kind: InstrKind::ShadowLoad {
                    dst_local: ptr_local,
                    require_tag: false,
                    validate_ref: is_ref,
                },
            });
            insert_points.push(InsertPoint {
                bb: def_bb,
                stmt_idx: def_stmt_idx,
                insert_before: false,
                source_info,
                place: Place::from(ptr_local),
                kind: InstrKind::ShadowStore {
                    src_local: ptr_local,
                },
            });
            return;
        }
        tagged_ptr_locals.insert(ptr_local);
        ptr_locals_needing_tag.insert(ptr_local);
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before: true,
            source_info,
            place: Place::from(ptr_local),
            kind: if is_ref {
                InstrKind::RetRoot {
                    dst_local: ptr_local,
                    is_mut,
                    is_ref: true,
                }
            } else {
                InstrKind::RawRoot {
                    ptr_local,
                    is_mut,
                    exposed_provenance: false,
                }
            },
        });
    }

    pub(in crate::instrumentation) fn scan_statement<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        stmt_idx: usize,
        stmt: &Statement<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        byte_copy_src_for_local: &mut HashMap<Local, Place<'tcx>>,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        projected_reborrow_anchor_specs: &mut ReborrowAnchorSpecMap,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
        summary_elidable_shared_call_ref_locals: &HashSet<Local>,
        call_only_reborrow_forward_sources: &HashMap<Local, Local>,
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
        trace_ssa_anchor: bool,
    ) {
        // Stack allocation lifetime: StorageLive/StorageDead.
        match stmt.kind {
            StatementKind::StorageDead(local) => {
                byte_copy_src_for_local.remove(&local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, local);
                let ty = body.local_decls[local].ty;
                // StorageDead does not write bytes. Shadow is killed by assignments/stores so
                // overlapping stack locals do not erase each other's live pointer fields.
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // NOTE: optimized MIR can place StorageDead before the last use
                    // through an outstanding reference. Emitting dead-by-default here
                    // causes false UAFs, so this remains opt-in.
                    if !self.use_storage_dead_enabled() {
                        return;
                    }
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: false,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                false,
                                &size_op,
                                "StorageDead",
                            );
                        }
                    }
                    return;
                }
            }
            StatementKind::StorageLive(local) => {
                let ty = body.local_decls[local].ty;
                // StorageLive reserves a local but does not initialize it. Actual writes below
                // are responsible for clearing or copying byte-level shadow.
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // Record pointer-typed locals only if their address is taken (interesting locals).
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: true,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                true,
                                &size_op,
                                "StorageLive",
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                byte_copy_src_for_local.remove(&dst_local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, dst_local);
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_pointer_ty(dst_ty)
                    && self.rhs_carries_boundary_recovered_ptr(
                        body,
                        rvalue,
                        boundary_recovered_ptr_locals,
                    )
                {
                    boundary_recovered_ptr_locals.insert(dst_local);
                }
                if self.is_one_byte_sized_ty(tcx, dst_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_one_byte_sized_ty(tcx, src_ty)
                            && self.place_contains_deref(src_place)
                        {
                            byte_copy_src_for_local.insert(dst_local, src_place);
                        }
                    }
                }
            }
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
                        // Skip reads through &'static references. These point to global memory
                        // that we don't track, and treating them as wild would be a false positive.
                        let skip_static_ref_read =
                            matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                        let skip_vtable_read = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                        if !skip_static_ref_read && !skip_vtable_read && self.is_pointer_ty(ptr_ty)
                        {
                            // Best-effort size: use the type of the dereferenced/read place.
                            let loaded_ty = p.ty(&body.local_decls, tcx).ty;
                            let read_ty = loaded_ty;
                            // Loading function pointers or vtable-like structs should not trigger
                            // memory access checks; treat these as benign metadata reads.
                            let skip_fn_ptr_read =
                                matches!(loaded_ty.kind(), TyKind::FnPtr(..) | TyKind::FnDef(..))
                                    || self.is_fn_table_adt_ty(tcx, loaded_ty);
                            let skip_vtable_field_read = match read_ty.kind() {
                                TyKind::FnPtr(..) | TyKind::FnDef(..) => true,
                                TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                                    self.is_fn_table_adt_ty(tcx, *pointee)
                                }
                                _ => self.is_fn_table_adt_ty(tcx, read_ty),
                            };
                            // Keep function-pointer and vtable-like metadata loads silent, but
                            // still instrument pointer-valued memory reads. We need those for
                            // unaligned ptr-to-ptr loads and similar cases where the loaded value
                            // is itself a pointer and the UB happens at the read.
                            if skip_fn_ptr_read || skip_vtable_field_read {
                                // Skip only the READ instrumentation; continue scanning this stmt.
                            } else {
                                let size_op = self.size_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
                                    loaded_ty,
                                    stmt.source_info.span,
                                );
                                let align_op = self.align_operand_for_deref_access(
                                    tcx,
                                    body,
                                    p.clone(),
                                    read_ty,
                                    stmt.source_info.span,
                                );

                                self.ensure_raw_root_before(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    stmt.source_info,
                                    ptr_local,
                                    insert_points,
                                    ptr_locals_needing_tag,
                                    tagged_ptr_locals,
                                    ptr_locals_with_tag_sources,
                                );
                                ptr_locals_needing_tag.insert(ptr_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: true,
                                    source_info: stmt.source_info,
                                    place: p.clone(),
                                    kind: InstrKind::PtrRead {
                                        ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });
                            }
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
                // Skip writes through &'static references. They are immutable by type,
                // and we don't track global memory for validity.
                let skip_static_ref_write =
                    matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                let skip_vtable_write = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                if !skip_static_ref_write && !skip_vtable_write && self.is_pointer_ty(ptr_ty) {
                    // Best-effort size: use the type of the *place being written* (after projections).
                    // This yields the correct size for patterns like `(*p).field = ...` or `(*p)[i] = ...`.
                    let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                    let size_op = self.size_operand_for_deref(
                        tcx,
                        body,
                        ptr_local,
                        lhs_ty,
                        stmt.source_info.span,
                    );
                    let align_op = self.align_operand_for_deref_access(
                        tcx,
                        body,
                        lhs_place.clone(),
                        lhs_ty,
                        stmt.source_info.span,
                    );

                    self.ensure_raw_root_before(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        ptr_local,
                        insert_points,
                        ptr_locals_needing_tag,
                        tagged_ptr_locals,
                        ptr_locals_with_tag_sources,
                    );
                    ptr_locals_needing_tag.insert(ptr_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: true,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::PtrWrite {
                            ptr_local,
                            size_op,
                            align_op,
                        },
                    });
                }
            }
        }

        // Direct stack-slot write: assignment to a tracked local/field without an initial Deref.
        // Use the local's reborrow anchor as the access tag so writes through the root local
        // invalidate stale children once that local has been borrowed. Keep this allow-untagged:
        // most locals are never borrowed, and a zero anchor should stay silent.
        if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
            let begins_with_deref = lhs_place
                .projection
                .iter()
                .next()
                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
            if !begins_with_deref {
                let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                if interesting_stack_locals.contains(&lhs_place.local)
                    && !self.is_pointer_ty(lhs_ty)
                {
                    let size_op =
                        self.size_operand_for_ty(tcx, body, lhs_ty, stmt.source_info.span);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::StackSlotWriteAllowUntagged {
                            place: lhs_place.clone(),
                            size_op,
                            align_op: self.align_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
                    });
                }
            }
        }

        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place))
                | Rvalue::CopyForDeref(src_place) = rvalue
                {
                    if self.is_std_fs_read_ok_vec_payload(tcx, body, *src_place, dst_ty)
                        && self.push_external_vec_u8_owner_import(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            false,
                            stmt.source_info,
                            dst_local,
                            insert_points,
                        )
                    {
                        return;
                    }
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
                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                        if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_base_ty = body.local_decls[src_place.local].ty;
                            let shadow_load_ok = self.is_shadowable_ptr_ty(tcx, body, src_ty)
                                && (!matches!(src_ty.kind(), TyKind::Ref(..))
                                    || self.place_contains_deref(src_place)
                                    || !self.is_pointer_ty(src_base_ty));
                            // A projected pointer field is a stored pointer value. Copying it into
                            // a local should transport that field's shadow, not synthesize a new
                            // borrow from the outer carrier.
                            if !src_place.projection.is_empty() && shadow_load_ok {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowLoad dst={:?} src={:?}",
                                    dst_local,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: src_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: self.place_contains_deref(src_place),
                                    },
                                });
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                                return;
                            }
                        }
                    }

                    let mut skip_tag_prop = false;

                    // Preserve lineage for direct projected pointer/reference copies like
                    // `_dst = copy (_agg.1: &mut T)`. Recovering only a base local here is often
                    // too coarse and can leave the destination untagged until first use.
                    let direct_projected_src_place = match rvalue {
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
                    };
                    if let Some(src_place) = direct_projected_src_place {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if !src_place.projection.is_empty() && self.is_pointer_ty(src_ty) {
                            if self.log_enabled(PassLogLevel::Trace) {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][projected-src] dst={:?} src={:?} dst_ty={:?}",
                                    dst_local,
                                    src_place,
                                    dst_ty
                                );
                            }
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: src_place,
                                kind: InstrKind::ShadowLoad {
                                    dst_local,
                                    require_tag: false,
                                    validate_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                },
                            });
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowStore {
                                    src_local: dst_local,
                                },
                            });
                            skip_tag_prop = true;
                        }
                    }

                    // Casts to raw pointers should create a fresh raw tag with parent lineage,
                    // rather than copying the source tag directly.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        if let TyKind::RawPtr(_pointee, mutbl) = dst_ty.kind() {
                            if let Some(src_place) = self.place_from_operand(op) {
                                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                if self.is_pointer_ty(src_ty) {
                                    let is_mut = matches!(mutbl, Mutability::Mut);
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_place.local);
                                    tagged_ptr_locals.insert(dst_local);
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place.clone(),
                                        },
                                    });
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
                                    skip_tag_prop = true;
                                }
                            }
                        }
                    }

                    // Pointer-from-non-pointer casts (including exposed provenance and transmute)
                    // drop lineage unless we synthesize a fresh root tag for the destination.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr
                        | CastKind::PointerCoercion(_, _)
                        | CastKind::Transmute
                        | CastKind::PointerWithExposedProvenance,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        let src_ty = op.ty(&body.local_decls, tcx);
                        let explicit_nonnull_transmute =
                            matches!(rvalue, Rvalue::Cast(CastKind::Transmute, ..))
                                && matches!(
                                    src_ty.kind(),
                                    TyKind::Adt(adt, _)
                                        if {
                                            let name = tcx.def_path_str(adt.did());
                                            name.contains("::NonNull") || name.contains("::Unique")
                                        }
                                );
                        if !self.is_pointer_ty(src_ty)
                            && !explicit_nonnull_transmute
                            && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                        {
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            let anchor_key = self.normalized_ptr_expr_key_for_rvalue(
                                body,
                                rvalue,
                                &block_data.statements,
                                stmt_idx,
                            );
                            let reused_anchor_state =
                                anchor_key.as_ref().and_then(|(key, _deps)| {
                                    self.reusable_ssa_anchor_for_expr(
                                        body,
                                        ssa_anchor_for_expr,
                                        key,
                                        dst_local,
                                        dst_ty,
                                    )
                                });
                            if let Some(anchor_state) = reused_anchor_state {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] reuse root-cast dst={:?} anchor={:?} source={:?} key={}",
                                        dst_local,
                                        anchor_state.local,
                                        anchor_state.source,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: match anchor_state.source {
                                        SsaAnchorSource::Tag => InstrKind::TagProp {
                                            dst: dst_local,
                                            src: anchor_state.local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                        SsaAnchorSource::RefAncestor => {
                                            InstrKind::TagPropFromRefAncestor {
                                                dst: dst_local,
                                                src: anchor_state.local,
                                            }
                                        }
                                    },
                                });
                            } else {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] store root-cast dst={:?} key={}",
                                        dst_local,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: true,
                                    },
                                });
                                if let Some((key, deps)) = anchor_key {
                                    ssa_anchor_for_expr.insert(
                                        key,
                                        SsaAnchorState {
                                            local: dst_local,
                                            deps,
                                            source: SsaAnchorSource::Tag,
                                        },
                                    );
                                }
                            }
                            skip_tag_prop = true;
                        }
                    }
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        // CopyForDeref shows up when MIR materializes a place for deref;
                        // it still represents a pointer value that needs tag propagation.
                        Rvalue::CopyForDeref(p) => p.as_local(),
                        // Wide-pointer construction can appear as aggregate from (data_ptr, metadata),
                        // e.g. `*mut [T] from (copy _data, copy _len)`. Preserve lineage from field 0.
                        Rvalue::Aggregate(_kind, ops) => ops
                            .iter()
                            .next()
                            .and_then(|op| self.place_from_operand(op))
                            .and_then(|p| p.as_local()),
                        // Pointer arithmetic lowering can appear as `Offset` binary ops.
                        Rvalue::BinaryOp(BinOp::Offset, ops) => {
                            self.place_from_operand(&ops.0).and_then(|p| p.as_local())
                        }
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute,
                            op,
                            _to_ty,
                        ) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        _ => None,
                    };

                    let projected_ptr_place = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op),
                        Rvalue::BinaryOp(BinOp::Offset, ops) => self.place_from_operand(&ops.0),
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute
                            | CastKind::PointerWithExposedProvenance,
                            op,
                            _,
                        ) => self.place_from_operand(op),
                        _ => None,
                    };

                    let mut src_local_opt = src_local_opt;
                    if src_local_opt.is_none() {
                        if let Some(p) = projected_ptr_place {
                            src_local_opt = self.recover_pointer_source_local_for_projected_place(
                                tcx, body, bb, stmt_idx, p, false,
                            );
                        }
                    }

                    if !skip_tag_prop {
                        let mut handled_ptr_tag = false;
                        if let Some(src_local) = src_local_opt {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                ptr_locals_needing_tag.insert(src_local);

                                let rhs_requires_retag = match rvalue {
                                    // Pointer arithmetic and projection-heavy pointer materialization
                                    // can change the pointee address; copying the source tag directly
                                    // keeps stale pointee metadata. Retag derived values instead.
                                    Rvalue::BinaryOp(op, _)
                                        if matches!(
                                            *op,
                                            BinOp::Offset | BinOp::Add | BinOp::Sub
                                        ) =>
                                    {
                                        true
                                    }
                                    Rvalue::Cast(CastKind::PointerCoercion(_, _), _, _)
                                        if self.ref_pointer_coercion_preserves_tag(
                                            tcx, body, rvalue, dst_ty,
                                        ) =>
                                    {
                                        false
                                    }
                                    Rvalue::CopyForDeref(_)
                                    | Rvalue::Cast(
                                        CastKind::PtrToPtr
                                        | CastKind::PointerCoercion(_, _)
                                        | CastKind::Transmute,
                                        _,
                                        _,
                                    )
                                    | Rvalue::Aggregate(_, _) => true,
                                    Rvalue::Use(op) => match op {
                                        Operand::Copy(p) | Operand::Move(p) => {
                                            !p.projection.is_empty()
                                        }
                                        _ => false,
                                    },
                                    _ => false,
                                };

                                if rhs_requires_retag {
                                    let anchor_key = projected_ptr_place.and_then(|src_place| {
                                        self.projected_reborrow_anchor_allowed_for_src_place(
                                            body, src_place,
                                        )
                                        .then(|| {
                                            self.normalized_ptr_copy_anchor_key(
                                                tcx,
                                                body,
                                                rvalue,
                                                &block_data.statements,
                                                stmt_idx,
                                            )
                                        })
                                        .flatten()
                                    });
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });

                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse ptr-derive dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store ptr-derive dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::PtrDerive {
                                                dst: dst_local,
                                                src: src_local,
                                                is_mut: self.ptr_is_mut(dst_ty),
                                                is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                                strict_validity: false,
                                            },
                                        });
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::TagProp {
                                            dst: dst_local,
                                            src: src_local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                    });
                                    self.rebind_ssa_anchors_for_copy(
                                        ssa_anchor_for_expr,
                                        src_local,
                                        dst_local,
                                    );
                                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::PtrUse {
                                                ptr_local: dst_local,
                                            },
                                        });
                                    }
                                }
                                tagged_ptr_locals.insert(dst_local);
                                if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::ShadowStore {
                                            src_local: dst_local,
                                        },
                                    });
                                }
                                handled_ptr_tag = true;
                            }
                        }
                        if !handled_ptr_tag {
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

                            if matches!(rvalue, Rvalue::CopyForDeref(_))
                                && self.log_enabled(PassLogLevel::Trace)
                            {
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
                                // The destination local is initialized by this assignment.
                                // Synthesize a root tag *after* the assignment so later uses
                                // (including call operands) do not see UNKNOWN_TAG. Preserve
                                // reference semantics for projected `&T` / `&mut T` copies;
                                // rooting them as raw pointers loses ref-kind behavior and can
                                // desynchronize later access metadata from the actual pointee.
                                let projected_src_place = match rvalue {
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
                                };
                                if projected_src_place.is_some()
                                    || self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                                {
                                    let is_mut = self.ptr_is_mut(dst_ty);
                                    let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                                    let anchor_key = self.normalized_ptr_copy_anchor_key(
                                        tcx,
                                        body,
                                        rvalue,
                                        &block_data.statements,
                                        stmt_idx,
                                    );
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });
                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse projected-root dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store projected-root dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        if let Some(src_place) = projected_src_place {
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: src_place,
                                                kind: InstrKind::ShadowLoad {
                                                    dst_local,
                                                    require_tag: false,
                                                    validate_ref: is_ref,
                                                },
                                            });
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: Place::from(dst_local),
                                                kind: InstrKind::ShadowStore {
                                                    src_local: dst_local,
                                                },
                                            });
                                        } else {
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: Place::from(dst_local),
                                                kind: if is_ref {
                                                    InstrKind::RetRoot {
                                                        dst_local,
                                                        is_mut,
                                                        is_ref: true,
                                                    }
                                                } else {
                                                    InstrKind::RawRoot {
                                                        ptr_local: dst_local,
                                                        is_mut,
                                                        exposed_provenance: matches!(
                                                            rvalue,
                                                            Rvalue::Cast(
                                                                CastKind::PointerWithExposedProvenance,
                                                                ..,
                                                            )
                                                        ),
                                                    }
                                                },
                                            });
                                        }
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
                                    tagged_ptr_locals.insert(dst_local);
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
                                }
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
                                        if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx: stmt_idx + 1,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: Place::from(dst_local),
                                                kind: InstrKind::RawRoot {
                                                    ptr_local: dst_local,
                                                    is_mut,
                                                    exposed_provenance: false,
                                                },
                                            });
                                            tagged_ptr_locals.insert(dst_local);
                                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                                insert_points.push(InsertPoint {
                                                    bb,
                                                    stmt_idx,
                                                    insert_before: false,
                                                    source_info: stmt.source_info,
                                                    place: Place::from(dst_local),
                                                    kind: InstrKind::ShadowStore {
                                                        src_local: dst_local,
                                                    },
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Pointer-shadow maintenance for memory-resident pointer slots.
        if let StatementKind::Assign(box (lhs_place, rvalue)) = &stmt.kind {
            let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;

            if !lhs_place.projection.is_empty() {
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place)) = rvalue
                {
                    if src_place.ty(&body.local_decls, tcx).ty == lhs_ty {
                        let lhs_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, lhs_ty, 3);
                        let dst_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *lhs_place, lhs_ty);
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, lhs_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowStore dst_field={:?} src_local={:?}",
                                        dst_field,
                                        src_field.local
                                    );
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowCopySlot dst_field={:?} src={:?}",
                                        dst_field,
                                        src_field
                                    );
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                            return;
                        }
                        if lhs_has_ptr_fields && lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Projected Aggregate ShadowCopyRange dst={:?} src={:?}",
                                lhs_place,
                                src_place
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                            return;
                        }
                    }
                }

                if self.place_contains_deref(lhs_place.clone())
                    && self.is_one_byte_sized_ty(tcx, lhs_ty)
                {
                    let byte_copy_src = match rvalue {
                        Rvalue::Use(Operand::Copy(src_place))
                        | Rvalue::Use(Operand::Move(src_place)) => src_place
                            .as_local()
                            .and_then(|src_local| byte_copy_src_for_local.get(&src_local).copied()),
                        _ => None,
                    };
                    if let Some(src_place) = byte_copy_src {
                        rz_pass_trace!(
                            self,
                            "[rusteze][ptr-shadow] ShadowCopyRange dst={:?} src={:?} size=1",
                            lhs_place,
                            src_place
                        );
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowCopyRange {
                                src_place,
                                size_op: SizeOperand::Const(self.const_usize(
                                    tcx,
                                    stmt.source_info.span,
                                    1,
                                )),
                            },
                        });
                        return;
                    }
                }

                if self.is_shadowable_ptr_ty(tcx, body, lhs_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_shadowable_ptr_ty(tcx, body, src_ty) {
                            if src_place.projection.is_empty() {
                                ptr_locals_needing_tag.insert(src_place.local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowStore dst={:?} src_local={:?}",
                                    lhs_place,
                                    src_place.local
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowStore {
                                        src_local: src_place.local,
                                    },
                                });
                            } else {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowCopySlot dst={:?} src={:?}",
                                    lhs_place,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowCopySlot { src_place },
                                });
                            }
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    } else {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowKill {
                                size_op: self.size_operand_for_ty(
                                    tcx,
                                    body,
                                    lhs_ty,
                                    stmt.source_info.span,
                                ),
                            },
                        });
                    }
                } else if lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
                    });
                }
            } else if let Some(dst_local) = lhs_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place))
                | Rvalue::CopyForDeref(src_place) = rvalue
                {
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx,
                                body,
                                src_place.local,
                                dst_ty,
                            )
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                        }
                    }
                }
                match rvalue {
                    Rvalue::Aggregate(kind, ops) => {
                        let ops_vec: Vec<Operand<'tcx>> = ops.iter().cloned().collect();
                        self.emit_aggregate_shadow_ops(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            stmt.source_info,
                            dst_local,
                            dst_ty,
                            kind,
                            &ops_vec,
                            insert_points,
                            ptr_locals_needing_tag,
                        );
                    }
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place))
                    | Rvalue::CopyForDeref(src_place)
                        if src_place.ty(&body.local_decls, tcx).ty == dst_ty =>
                    {
                        let dst_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, dst_ty, 3);
                        let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(dst_local),
                            dst_ty,
                        );
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, dst_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                        } else if dst_has_ptr_fields && dst_ty.is_sized(tcx, body.typing_env(tcx)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Wrapper-carrier transmute: optimized MIR frequently wraps a pointer local into a
        // `NonNull<T>`/`Unique<T>`-style carrier via `Transmute`. Restore the destination leaf
        // shadow so later projected loads from the wrapper field do not degrade to UNKNOWN_TAG.
        if let StatementKind::Assign(box (dst_place, Rvalue::Cast(CastKind::Transmute, op, _))) =
            &stmt.kind
        {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    );
                    if !dst_leafs.is_empty() {
                        if let Some(src_place) = self.place_from_operand(op) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_leafs = self
                                .shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, src_ty);
                            if let Some(matched_leafs) =
                                self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                            {
                                for (dst_spec, src_spec) in matched_leafs {
                                    let dst_leaf = dst_spec.place;
                                    let src_leaf = src_spec.place;
                                    let kind = if src_leaf.projection.is_empty()
                                        && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                    {
                                        ptr_locals_needing_tag.insert(src_leaf.local);
                                        InstrKind::ShadowStore {
                                            src_local: src_leaf.local,
                                        }
                                    } else {
                                        InstrKind::ShadowCopySlot {
                                            src_place: src_leaf,
                                        }
                                    };
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: dst_leaf,
                                        kind,
                                    });
                                }
                            } else if self.ty_contains_pointer_fields(tcx, body, src_ty, 3)
                                && dst_ty.is_sized(tcx, body.typing_env(tcx))
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowKill {
                                        size_op: self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            dst_ty,
                                            stmt.source_info.span,
                                        ),
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Handle pointer constants embedded in aggregate/field assignments where the destination
        // is not a pointer local we can tag. We still want to record the backing global allocation
        // so later derefs (e.g., vtable loads) do not report WILD_POINTER.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            let dst_is_local = dst_place.as_local().is_some();
            let mut const_ops: Vec<&ConstOperand<'tcx>> = Vec::new();

            match rvalue {
                Rvalue::Aggregate(_, ops) => {
                    for op in ops.iter() {
                        if let Operand::Constant(c) = op {
                            const_ops.push(c);
                        }
                    }
                }
                Rvalue::Use(Operand::Constant(c)) | Rvalue::Cast(_, Operand::Constant(c), _) => {
                    if !dst_is_local {
                        const_ops.push(c);
                    }
                }
                _ => {}
            }

            if !const_ops.is_empty() {
                let place_local = dst_place.as_local().unwrap_or(RETURN_PLACE);
                for c in const_ops {
                    if let Some(info) = self.const_alloc_info(tcx, c) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(place_local),
                            kind: InstrKind::ConstAllocConst {
                                const_op: c.clone(),
                                size: info.size,
                                base_offset: info.base_offset,
                            },
                        });
                    }
                }
            }
        }

        // std/alloc pattern where a thin pointer is produced by `Transmute` from
        // `NonNull<T>`/`Unique<T>` (ADT). TagProp does not apply because the source is not
        // necessarily a thin pointer local. Prefer deriving from a recovered pointer source
        // local (to keep lineage), and fall back to RawRoot only when recovery fails.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    if let Rvalue::Cast(CastKind::Transmute, op, _to_ty) = rvalue {
                        let src_ty = op.ty(body, tcx);
                        let is_nonnull_like = match src_ty.kind() {
                            TyKind::Adt(adt, _) => {
                                let name = tcx.def_path_str(adt.did());
                                name.contains("::NonNull") || name.contains("::Unique")
                            }
                            _ => false,
                        };

                        if is_nonnull_like {
                            let is_mut = match dst_ty.kind() {
                                TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                _ => false,
                            };
                            let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                            let src_place_opt = self.place_from_operand(op);
                            let direct_field_shadow = src_place_opt
                                .and_then(|src_place| src_place.as_local())
                                .and_then(|src_local| {
                                    self.first_direct_pointer_field_place(tcx, body, src_local)
                                });
                            if let Some(field_place) = direct_field_shadow {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: field_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: false,
                                    },
                                });
                                return;
                            }
                            let src_local_opt = src_place_opt
                                .and_then(|src_place| {
                                    if matches!(
                                        rvalue,
                                        Rvalue::Cast(CastKind::PointerWithExposedProvenance, ..,)
                                    ) {
                                        self.backtrack_global_exposed_provenance_source_local(
                                            body, dst_local,
                                        )
                                        .or_else(|| {
                                            src_place.as_local().and_then(|src_local| {
                                                if self
                                                    .is_pointer_ty(body.local_decls[src_local].ty)
                                                {
                                                    Some(src_local)
                                                } else {
                                                    self.backtrack_pointer_source_local(
                                                        body,
                                                        src_local,
                                                        &block_data.statements[..stmt_idx],
                                                    )
                                                    .or_else(|| {
                                                        self.backtrack_global_pointer_value_local(
                                                            body, src_local,
                                                        )
                                                    })
                                                }
                                            })
                                        })
                                    } else if let Some(src_local) = src_place.as_local() {
                                        if self.is_pointer_ty(body.local_decls[src_local].ty) {
                                            Some(src_local)
                                        } else {
                                            self.backtrack_pointer_source_local(
                                                body,
                                                src_local,
                                                &block_data.statements[..stmt_idx],
                                            )
                                            .or_else(
                                                || {
                                                    self.backtrack_global_pointer_value_local(
                                                        body, src_local,
                                                    )
                                                },
                                            )
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .and_then(|src_local| {
                                    let Some(src_place) = src_place_opt else {
                                        return Some(src_local);
                                    };
                                    let src_place_ty = src_place.ty(&body.local_decls, tcx).ty;
                                    let projected_carrier_load = !self.is_pointer_ty(src_place_ty)
                                        && !src_place.projection.is_empty()
                                        && src_local == src_place.local;
                                    if projected_carrier_load {
                                        None
                                    } else {
                                        Some(src_local)
                                    }
                                });

                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            if let Some(src_local) = src_local_opt {
                                ptr_locals_needing_tag.insert(src_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::PtrDerive {
                                        dst: dst_local,
                                        src: src_local,
                                        is_mut,
                                        is_ref,
                                        strict_validity: false,
                                    },
                                });
                            } else if let Some(src_place) = src_place_opt {
                                if !is_ref {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place,
                                        },
                                    });
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::RawRoot {
                                            ptr_local: dst_local,
                                            is_mut,
                                            exposed_provenance: false,
                                        },
                                    });
                                }
                            } else {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: false,
                                    },
                                });
                            }
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
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    let (binop, lhs_op) = match rvalue {
                        Rvalue::BinaryOp(op, box (lhs, _rhs)) => (Some(*op), Some(lhs)),
                        // Newer nightlies no longer have `Rvalue::CheckedBinaryOp`. The checked/overflowing
                        // forms lower to regular `BinaryOp` + extra logic, so handling `BinaryOp` is enough
                        // for our pointer-derive tagging purposes here.
                        _ => (None, None),
                    };

                    if let (Some(op), Some(lhs)) = (binop, lhs_op) {
                        // Raw pointer arithmetic frequently lowers to `BinOp::Offset`.
                        // Treat it like Add/Sub for tag-derivation so the derived pointer
                        // local does not remain untagged.
                        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Offset) {
                            if let Some(src_place) = self.place_from_operand(lhs) {
                                let src_local = src_place.local;
                                let src_ty = body.local_decls[src_local].ty;
                                if self.is_thin_ptr_ty(tcx, body, src_ty) {
                                    let is_mut = match dst_ty.kind() {
                                        TyKind::Ref(_, _ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
                                        TyKind::RawPtr(_ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
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
                                        kind: InstrKind::PtrDerive {
                                            dst: dst_local,
                                            src: src_local,
                                            is_mut,
                                            is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                            strict_validity: true,
                                        },
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
                let black_box_sink_ref_temp = if matches!(bk, BorrowKind::Shared)
                    && stmt_idx + 1 == block_data.statements.len()
                {
                    if let Some(Terminator {
                        kind:
                            TerminatorKind::Call {
                                func,
                                args,
                                destination,
                                ..
                            },
                        ..
                    }) = &block_data.terminator
                    {
                        let call_dest_loc = Location {
                            block: bb,
                            statement_index: block_data.statements.len(),
                        };
                        let call_returns_dead = destination.as_local().is_some_and(|local| {
                            !self.local_has_observable_use_excluding_call_dest(
                                body,
                                local,
                                call_dest_loc,
                            )
                        });
                        let call_is_black_box = self
                            .direct_callee(tcx, body, block_data, func)
                            .map(|(did, _)| tcx.def_path_str(did).contains("black_box"))
                            .unwrap_or(false);
                        let arg_matches = args.iter().any(|arg| {
                            self.place_from_operand(&arg.node)
                                .is_some_and(|arg_place| arg_place.as_local() == Some(lhs_local))
                        });
                        call_returns_dead && call_is_black_box && arg_matches
                    } else {
                        false
                    }
                } else {
                    false
                };
                if self.is_pointer_ty(lhs_ty)
                    && !summary_elidable_shared_call_ref_locals.contains(&lhs_local)
                    && !call_only_reborrow_forward_sources.contains_key(&lhs_local)
                    && !black_box_sink_ref_temp
                {
                    let allow_projected_anchor =
                        self.projected_reborrow_anchor_allowed_for_src_place(body, *src_place);
                    let anchor_key = allow_projected_anchor
                        .then(|| {
                            self.normalized_ptr_expr_key_for_ref_source_place(
                                body,
                                *src_place,
                                &block_data.statements,
                                stmt_idx,
                            )
                        })
                        .flatten();
                    let reused_anchor_state = if allow_projected_anchor
                        && self.allow_ssa_anchor_reuse_for_ref_source_place(body, *bk, *src_place)
                    {
                        anchor_key.as_ref().and_then(|(key, _deps)| {
                            self.reusable_ssa_anchor_for_ref_source_expr(
                                body,
                                ssa_anchor_for_expr,
                                key,
                                lhs_local,
                            )
                        })
                    } else {
                        None
                    };
                    let ref_src_place = reused_anchor_state
                        .as_ref()
                        .map(|anchor_state| Place::from(anchor_state.local))
                        .unwrap_or(*src_place);
                    let projected_reborrow_anchor_key = self.maybe_projected_reborrow_anchor_key(
                        body,
                        *src_place,
                        anchor_key.as_ref(),
                        projected_reborrow_anchor_specs,
                    );

                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Ref {
                            bk: *bk,
                            src: ref_src_place,
                            projected_reborrow_anchor_key,
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    if let Some(anchor_state) = reused_anchor_state.as_ref() {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] reuse ref parent dst={:?} anchor={:?} source={:?} key={}",
                                lhs_local,
                                anchor_state.local,
                                anchor_state.source,
                                anchor_key
                                    .as_ref()
                                    .map(|(key, _)| key.as_str())
                                    .unwrap_or("<none>")
                            );
                        }
                    }
                    if let Some((key, deps)) = anchor_key {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] store ref-ancestor dst={:?} key={}",
                                lhs_local,
                                key
                            );
                        }
                        ssa_anchor_for_expr.insert(
                            key,
                            SsaAnchorState {
                                local: lhs_local,
                                deps,
                                source: SsaAnchorSource::RefAncestor,
                            },
                        );
                    }
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
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
                        kind: InstrKind::Raw {
                            is_mut,
                            src: src_place.clone(),
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
                }
            }
        }
    }
}
