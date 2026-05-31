//! Lowers scheduled instrumentation hooks into concrete MIR statements and blocks.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn materialize_size_operand<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        size_op: &SizeOperand<'tcx>,
    ) -> (Operand<'tcx>, Vec<Statement<'tcx>>) {
        match size_op {
            SizeOperand::Const(op) => (op.clone(), Vec::new()),
            SizeOperand::SizeOf(ty) => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(size_local)), vec![stmt])
            }
            SizeOperand::RefSizedBoundsLen(ty) => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_usize_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let zst_marker_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let encoded_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *ty),
                    ))),
                );
                let is_zero_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(is_zero_local),
                        Rvalue::BinaryOp(
                            BinOp::Eq,
                            Box::new((
                                Operand::Copy(Place::from(size_local)),
                                self.const_usize(tcx, source_info.span, 0),
                            )),
                        ),
                    ))),
                );
                let is_zero_usize_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(is_zero_usize_local),
                        Rvalue::Cast(
                            CastKind::IntToInt,
                            Operand::Copy(Place::from(is_zero_local)),
                            tcx.types.usize,
                        ),
                    ))),
                );
                let zst_marker_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(zst_marker_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(is_zero_usize_local)),
                                self.const_usize(tcx, source_info.span, usize::MAX - 1),
                            )),
                        ),
                    ))),
                );
                let encoded_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(encoded_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(size_local)),
                                Operand::Copy(Place::from(zst_marker_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(encoded_local)),
                    vec![
                        size_stmt,
                        is_zero_stmt,
                        is_zero_usize_stmt,
                        zst_marker_stmt,
                        encoded_stmt,
                    ],
                )
            }
            SizeOperand::AlignOf(ty) => {
                let align_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(align_local),
                        Rvalue::NullaryOp(NullOp::AlignOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(align_local)), vec![stmt])
            }
            SizeOperand::ElemCount { elem_ty, count_op } => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((Operand::Copy(Place::from(size_local)), count_op.clone())),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataSlice { ptr_local, elem_ty } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(size_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![meta_stmt, size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataStr { ptr_local } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                (Operand::Copy(Place::from(meta_local)), vec![meta_stmt])
            }
            SizeOperand::PtrMetadataAdtSlice {
                ptr_local,
                adt_ty,
                field_idx,
                elem_ty,
            } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let elem_size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let tail_bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let head_off_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let total_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                let elem_size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(elem_size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let tail_bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(tail_bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(elem_size_local)),
                            )),
                        ),
                    ))),
                );
                let offset_of_list = tcx.mk_offset_of(&[(VariantIdx::from_u32(0), *field_idx)]);
                let head_off_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(head_off_local),
                        Rvalue::NullaryOp(NullOp::OffsetOf(offset_of_list), *adt_ty),
                    ))),
                );
                let total_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(total_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(head_off_local)),
                                Operand::Copy(Place::from(tail_bytes_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(total_local)),
                    vec![
                        meta_stmt,
                        elem_size_stmt,
                        tail_bytes_stmt,
                        head_off_stmt,
                        total_stmt,
                    ],
                )
            }
        }
    }

    pub(in crate::instrumentation) fn allocate_tag_locals<'tcx>(
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

    pub(in crate::instrumentation) fn allocate_u8_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        ptrs: HashSet<Local>,
    ) -> HashMap<Local, Local> {
        let mut local_for_ptr_local: HashMap<Local, Local> = HashMap::new();
        for ptr_local in ptrs.into_iter() {
            if !local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u8, rustc_span::DUMMY_SP));
                local_for_ptr_local.insert(ptr_local, t);
            }
        }
        local_for_ptr_local
    }

    pub(in crate::instrumentation) fn allocate_reborrow_anchor_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        anchor_specs: &ReborrowAnchorSpecMap,
    ) -> HashMap<String, Local> {
        let mut anchor_local_for_key: HashMap<String, Local> = HashMap::new();
        let mut keys: Vec<&String> = anchor_specs.keys().collect();
        keys.sort();
        for key in keys {
            let local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
            anchor_local_for_key.insert(key.clone(), local);
        }
        anchor_local_for_key
    }

    pub(in crate::instrumentation) fn ptr_state_locals_for_ptr_local(
        &self,
        ptr_local: Local,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
    ) -> Option<PtrStateLocals> {
        Some(PtrStateLocals {
            tag_local: tag_local_for_ptr_local.get(&ptr_local).copied()?,
            ref_ancestor_local: ref_ancestor_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_parent_local: export_parent_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_recovered_local: export_parent_is_recovered_local_for_ptr_local
                .get(&ptr_local)
                .copied(),
        })
    }

    pub(in crate::instrumentation) fn carrier_slot_locals_for_local(
        &self,
        local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<CarrierSlotLocals> {
        Some(CarrierSlotLocals {
            anchor_local: reborrow_anchor_local_for_stack_local.get(&local).copied()?,
            slot_family_valid_local: anchor_is_slot_family_local_for_stack_local
                .get(&local)
                .copied(),
        })
    }

    pub(in crate::instrumentation) fn schedule_reborrow_anchor_resets<'tcx>(
        &self,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        reborrow_anchor_specs: &ReborrowAnchorSpecMap,
        reborrow_anchor_local_for_key: &HashMap<String, Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let source_info = stmt.source_info;
                let touched_local = match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        Some(local)
                    }
                    StatementKind::Assign(box (place, _)) => place.as_local(),
                    _ => None,
                };
                if let Some(local) = touched_local {
                    if let Some(slot_state) = self.carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info,
                            place: Place::from(slot_state.anchor_local),
                            kind: InstrKind::ReborrowAnchorZero {
                                anchor_local: slot_state.anchor_local,
                                anchor_state_local: slot_state.slot_family_valid_local,
                            },
                        });
                    }
                    for (key, deps) in reborrow_anchor_specs.iter() {
                        if deps.contains(&local) {
                            if let Some(anchor_local) =
                                reborrow_anchor_local_for_key.get(key).copied()
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info,
                                    place: Place::from(anchor_local),
                                    kind: InstrKind::ReborrowAnchorZero {
                                        anchor_local,
                                        anchor_state_local: None,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    pub(in crate::instrumentation) fn schedule_reborrow_anchor_propagation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        boundary_recovered_ptr_locals: &HashSet<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let Some(dst_slot_state) = self.carrier_slot_locals_for_local(
                    dst_local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };
                if let Rvalue::Aggregate(_, operands) = rvalue {
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
                    if let (Some(src_local), false) = (unique_src_local, ambiguous) {
                        if src_local != dst_local {
                            let src_ty = body.local_decls[src_local].ty;
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local,
                                    mark_slot_family: self
                                        .supports_call_boundary_anchor_local(tcx, body, dst_local)
                                        && boundary_recovered_ptr_locals.contains(&src_local)
                                        && matches!(
                                            src_ty.kind(),
                                            TyKind::Ref(_, _, Mutability::Not)
                                        ),
                                },
                            });
                            continue;
                        }
                    }
                }
                let src_place = match rvalue {
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place)) => *src_place,
                    _ => continue,
                };
                let src_local = src_place.local;
                if src_local == dst_local {
                    continue;
                }
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty)
                    && !src_place.projection.is_empty()
                    && self.supports_slot_family_local(tcx, body, dst_local)
                {
                    if let Some(recovered_src_local) = self
                        .backtrack_same_typed_call_result_source_local(tcx, body, src_local, dst_ty)
                    {
                        if recovered_src_local != src_local {
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
                            continue;
                        }
                    }
                }
                if !reborrow_anchor_local_for_stack_local.contains_key(&src_local) {
                    let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx, body, src_local, dst_ty,
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
                            continue;
                        }
                    }
                    if src_ty == dst_ty
                        && self.is_pointer_ty(body.local_decls[src_local].ty)
                        && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::ReborrowAnchorSet {
                                dst_local,
                                anchor_local: dst_slot_state.anchor_local,
                                anchor_state_local: dst_slot_state.slot_family_valid_local,
                                src_ptr_local: src_local,
                            },
                        });
                    }
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::ReborrowAnchorSeed {
                        dst_local,
                        src_local,
                        mark_slot_family: !src_place.projection.is_empty()
                            && self.supports_call_boundary_anchor_local(tcx, body, dst_local),
                    },
                });
            }
        }
    }

    pub(in crate::instrumentation) fn init_tag_locals_to_zero<'tcx>(
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
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));

            let zero: Operand<'tcx> = self.const_u64(tcx, source_info.span, 0);
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((Place::from(*tag_local), Rvalue::Use(zero)))),
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

    pub(in crate::instrumentation) fn init_extra_tag_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        tag_locals: &[Local],
    ) {
        if tag_locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for tag_local in tag_locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*tag_local),
                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                ))),
            ));
        }

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

    pub(in crate::instrumentation) fn init_extra_u8_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        locals: &[Local],
    ) {
        if locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for local in locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*local),
                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                ))),
            ));
        }

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

    /// Add holder acquire/release hooks around hidden tag-local writes after the main MIR
    /// instrumentation pass has materialized them.

    pub(in crate::instrumentation) fn schedule_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        manual_holder_managed_tag_assignments: &HashSet<(BasicBlock, usize)>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        let mut ptr_local_for_tag_local: HashMap<Local, Local> = HashMap::new();
        for (ptr_local, tag_local) in tag_local_for_ptr_local {
            ptr_local_for_tag_local.insert(*tag_local, *ptr_local);
        }

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                match stmt.kind {
                    StatementKind::Assign(box (dst_place, _)) => {
                        if manual_holder_managed_tag_assignments.contains(&(bb, stmt_idx)) {
                            continue;
                        }
                        let Some(dst_local) = dst_place.as_local() else {
                            continue;
                        };
                        let Some(ptr_local) = ptr_local_for_tag_local.get(&dst_local).copied()
                        else {
                            continue;
                        };
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagLocalKill {
                                tag_local: dst_local,
                            },
                        });
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                    StatementKind::StorageDead(dead_local) => {
                        if !tag_local_for_ptr_local.contains_key(&dead_local) {
                            continue;
                        }
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(dead_local),
                            kind: InstrKind::TagKill {
                                ptr_local: dead_local,
                            },
                        });
                    }
                    _ => {}
                }
            }

            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            let TerminatorKind::Call {
                destination,
                target,
                ..
            } = &term.kind
            else {
                continue;
            };
            if let Some(target_bb) = *target {
                if let Some(dst_local) = destination.as_local() {
                    if ptr_local_for_tag_local.contains_key(&dst_local) {
                        insert_points.push(InsertPoint {
                            bb: target_bb,
                            stmt_idx: 0,
                            insert_before: false,
                            source_info: term.source_info,
                            place: *destination,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                }
            }
        }
    }

    pub(in crate::instrumentation) fn schedule_aux_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        aux_tag_locals: &HashSet<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, _)) = stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                if !aux_tag_locals.contains(&dst_local) {
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: true,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagLocalKill {
                        tag_local: dst_local,
                    },
                });
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagRetain {
                        tag_local: dst_local,
                    },
                });
            }
        }
    }

    pub(in crate::instrumentation) fn insert_instrumentation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        insert_points: Vec<InsertPoint<'tcx>>,
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        fn resolve_return_chain_bb<'tcx>(body: &Body<'tcx>, mut bb: BasicBlock) -> BasicBlock {
            for _ in 0..64 {
                let Some(term) = body.basic_blocks[bb].terminator.as_ref() else {
                    break;
                };
                match &term.kind {
                    TerminatorKind::Return => break,
                    // End-of-block instrumentation on a return site can be applied after the
                    // block has already been split into a synthetic call chain. Follow the
                    // continuation so return-side hooks still land immediately before the
                    // real `Return`.
                    TerminatorKind::Call {
                        target: Some(next), ..
                    } if body.basic_blocks[bb].statements.is_empty() => {
                        bb = *next;
                    }
                    _ => break,
                }
            }
            bb
        }

        fn is_runtime_hook_call<'tcx>(
            tcx: TyCtxt<'tcx>,
            term: &Terminator<'tcx>,
        ) -> Option<BasicBlock> {
            let TerminatorKind::Call {
                func,
                target: Some(next),
                ..
            } = &term.kind
            else {
                return None;
            };

            let Operand::Constant(c) = func else {
                return None;
            };
            let TyKind::FnDef(def_id, _) = c.const_.ty().kind() else {
                return None;
            };
            (tcx.crate_name(def_id.krate).as_str() == "runtime").then_some(*next)
        }

        fn resolve_split_chain_insert_site<'tcx>(
            tcx: TyCtxt<'tcx>,
            body: &Body<'tcx>,
            orig_stmt_prefix_len: &HashMap<BasicBlock, usize>,
            mut bb: BasicBlock,
            mut stmt_idx: usize,
            insert_before: bool,
        ) -> (BasicBlock, usize) {
            for _ in 0..64 {
                let bd = &body.basic_blocks[bb];
                let len = orig_stmt_prefix_len
                    .get(&bb)
                    .copied()
                    .unwrap_or_else(|| bd.statements.len());
                let needs_follow = if insert_before {
                    stmt_idx > len
                } else {
                    stmt_idx >= len
                };
                if !needs_follow {
                    break;
                }
                let Some(term) = bd.terminator.as_ref() else {
                    break;
                };
                let Some(next) = is_runtime_hook_call(tcx, term) else {
                    break;
                };
                stmt_idx = stmt_idx.saturating_sub(len);
                bb = next;
            }
            (bb, stmt_idx)
        }

        fn instr_priority(kind: &InstrKind<'_>) -> u8 {
            match kind {
                InstrKind::Ref { .. }
                | InstrKind::Raw { .. }
                | InstrKind::RawRoot { .. }
                | InstrKind::FnEnter { .. }
                | InstrKind::ArgRetag { .. }
                | InstrKind::ArgAnchorTake { .. }
                | InstrKind::ArgLeafTake { .. }
                | InstrKind::RetRoot { .. }
                | InstrKind::PtrDerive { .. }
                | InstrKind::PtrDeriveParent { .. } => 0,
                // Tag propagation must execute after tag-creating hooks but before
                // access/usage hooks at the same insertion site.
                InstrKind::TagProp { .. }
                | InstrKind::TagPropFromRefAncestor { .. }
                | InstrKind::FnExit { .. }
                | InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. }
                | InstrKind::ReborrowAnchorSet { .. }
                | InstrKind::ReborrowAnchorSeed { .. }
                | InstrKind::ReborrowAnchorZero { .. }
                | InstrKind::ParentTagSnapshot { .. }
                | InstrKind::ArgAnchorSeedFromShadow { .. } => 1,
                InstrKind::ShadowLoad { .. } => 1,
                // Debug ref activation may need the restored tag/ref_ancestor emitted by
                // ShadowLoad or TagProp at the same definition site.
                InstrKind::DebugRefActivate { .. } => 2,
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. }
                // Return-boundary exports must run before FnExit tears down the callee frame.
                | InstrKind::RetAnchorPush { .. }
                | InstrKind::RetPush { .. }
                | InstrKind::RetLeafPush { .. }
                | InstrKind::RetAnchorRoot { .. }
                | InstrKind::MutArgRetPush { .. }
                | InstrKind::MutArgRetLeafPush { .. }
                | InstrKind::ShadowKill { .. } => 2,
                // Keep stack-slot writes in the same bucket as reborrow-anchor maintenance so
                // the later-scheduled anchor zeroing is inserted first and then moved after the
                // write call when we split the block at this statement.
                InstrKind::StackSlotWriteAllowUntagged { .. } => 1,
                InstrKind::ShadowStore { .. }
                | InstrKind::ShadowStoreBoxPointee { .. }
                | InstrKind::ShadowCopySlot { .. }
                | InstrKind::ShadowCopyRange { .. } => 3,
                InstrKind::IndirectCallScopeBegin => 2,
                InstrKind::CallArgPush { .. }
                | InstrKind::IndirectCallArgPush { .. }
                | InstrKind::CallArgValidate { .. }
                | InstrKind::CallArgLeafPush { .. }
                | InstrKind::IndirectCallArgLeafPush { .. }
                | InstrKind::PtrUse { .. }
                | InstrKind::RetValidate { .. }
                | InstrKind::RetAnchorTake { .. }
                | InstrKind::RetLeafTake { .. }
                | InstrKind::MutArgRetTake { .. }
                | InstrKind::MutArgRetLeafTake { .. }
                | InstrKind::MutArgRetTakePtrOnly { .. } => 3,
                InstrKind::IndirectCallScopeEnd => 4,
                _ => 4,
            }
        }

        // Entry-side arg retag/anchor initialization must run before any ptr reads/writes.
        // We split it out so we can enforce ordering independent of stmt_idx sorting.
        let mut arg_retag_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();
        let mut other_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();

        for (idx, ip) in insert_points.into_iter().enumerate() {
            if matches!(
                ip.kind,
                InstrKind::FnEnter { .. }
                    | InstrKind::ArgRetag { .. }
                    | InstrKind::ArgAnchorTake { .. }
                    | InstrKind::ArgAnchorSeedFromShadow { .. }
                    | InstrKind::ArgLeafTake { .. }
            ) {
                arg_retag_points.push((idx, ip));
            } else {
                other_points.push((idx, ip));
            }
        }

        let sort_points = |points: &mut Vec<(usize, InsertPoint<'tcx>)>| {
            points.sort_by_key(|(idx, ip)| {
                (ip.bb.index(), ip.stmt_idx, instr_priority(&ip.kind), *idx)
            });
        };

        sort_points(&mut other_points);
        sort_points(&mut arg_retag_points);

        let mut orig_stmt_prefix_len: HashMap<BasicBlock, usize> = HashMap::new();

        for (_idx, ip) in other_points.into_iter().rev() {
            let mut bb = ip.bb;
            let mut stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            let (resolved_bb, resolved_stmt_idx) = resolve_split_chain_insert_site(
                tcx,
                body,
                &orig_stmt_prefix_len,
                bb,
                stmt_idx,
                ip.insert_before,
            );
            bb = resolved_bb;
            stmt_idx = resolved_stmt_idx;

            if matches!(creation_kind, InstrKind::StackAlloc { live: false, .. })
                && !ip.insert_before
            {
                let resolved_bb = resolve_return_chain_bb(body, bb);
                if resolved_bb != bb {
                    bb = resolved_bb;
                    stmt_idx = body.basic_blocks[bb].statements.len();
                }
            }

            // Avoid emitting allow-untagged READ/WRITE for raw-pointer args from unknown calls.
            // These are a common source of false positives (e.g., pointer casts).
            if let InstrKind::PtrReadAllowUntagged { ptr_local, .. }
            | InstrKind::PtrWriteAllowUntagged { ptr_local, .. } = creation_kind
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if matches!(ptr_ty.kind(), TyKind::RawPtr(..)) {
                    continue;
                }
            }

            if let InstrKind::ShadowLoad {
                dst_local,
                require_tag,
                validate_ref,
            } = creation_kind.clone()
            {
                let Some(dst_ptr_state) = self.ptr_state_locals_for_ptr_local(
                    dst_local,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    export_parent_local_for_ptr_local,
                    export_parent_is_recovered_local_for_ptr_local,
                ) else {
                    continue;
                };
                let Some(dst_ref_ancestor_local) = dst_ptr_state.ref_ancestor_local else {
                    continue;
                };
                let dst_export_parent_local = dst_ptr_state.boundary_parent_local;
                let dst_recovered_local = dst_ptr_state.boundary_recovered_local;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let ptr_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((ptr_addr_stmt1_opt, ptr_addr_stmt2)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(dst_local),
                    ptr_addr_local,
                ) else {
                    continue;
                };

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let ref_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                let dst_ty = body.local_decls[dst_local].ty;
                let needs_loaded_ref_validation =
                    matches!(dst_ty.kind(), TyKind::Ref(..)) && (require_tag || validate_ref);
                let projected_field_ty = place.ty(&body.local_decls, tcx).ty;
                let projected_carrier_anchor = if !place.projection.is_empty()
                    && matches!(projected_field_ty.kind(), TyKind::Ref(..))
                    && !self.is_pointer_ty(body.local_decls[place.local].ty)
                {
                    reborrow_anchor_local_for_stack_local
                        .get(&place.local)
                        .copied()
                } else {
                    None
                };
                let post_load_block = if projected_carrier_anchor.is_some() {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else if needs_loaded_ref_validation {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else {
                    cont_block
                };
                let validate_block = if needs_loaded_ref_validation {
                    if projected_carrier_anchor.is_some() {
                        Some(
                            body.basic_blocks_mut()
                                .push(BasicBlockData::new(None, is_cleanup)),
                        )
                    } else {
                        Some(post_load_block)
                    }
                } else {
                    None
                };

                let tag_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_tag_for_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let tag_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ref_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_ref_ancestor_for_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let export_parent_func = dst_export_parent_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent_for_ptr,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let export_parent_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let recovered_func = dst_recovered_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent_recovered_for_ptr,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let recovered_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let final_post_load_block = post_load_block;
                let recovered_block = dst_recovered_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let export_parent_target = recovered_block.unwrap_or(final_post_load_block);
                let export_parent_block = dst_export_parent_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let ref_target = export_parent_block.unwrap_or(export_parent_target);

                body.basic_blocks_mut()[ref_block].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: ref_func,
                        args: ref_args,
                        destination: Place::from(dst_ref_ancestor_local),
                        target: Some(ref_target),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });

                if let (
                    Some(export_parent_block),
                    Some(export_parent_local),
                    Some(export_parent_func),
                ) = (
                    export_parent_block,
                    dst_export_parent_local,
                    export_parent_func,
                ) {
                    body.basic_blocks_mut()[export_parent_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: export_parent_func,
                            args: export_parent_args,
                            destination: Place::from(export_parent_local),
                            target: Some(export_parent_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let (Some(recovered_block), Some(recovered_local), Some(recovered_func)) =
                    (recovered_block, dst_recovered_local, recovered_func)
                {
                    body.basic_blocks_mut()[recovered_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: recovered_func,
                            args: recovered_args,
                            destination: Place::from(recovered_local),
                            target: Some(final_post_load_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let Some(anchor_local) = projected_carrier_anchor {
                    let tag_is_zero_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                    let validate_target = validate_block.unwrap_or(cont_block);
                    let repair_cont_block = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: validate_target,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[repair_cont_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_local))),
                            ))),
                        ));
                    if let Some(dst_export_parent_local) = dst_export_parent_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        dst_ptr_state.tag_local,
                                    ))),
                                ))),
                            ));
                    }
                    if let Some(dst_recovered_local) = dst_recovered_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                                ))),
                            ));
                    }

                    let repair_call_block = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup));
                    let repair_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (repair_addr_stmt1_opt, repair_addr_stmt2) = self
                        .addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(dst_local),
                            repair_addr_local,
                        )
                        .expect("ShadowLoad projected ref repair on non-pointer local");
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    let dst_ty = body.local_decls[dst_local].ty;
                    let is_mut_u8 = match dst_ty.kind() {
                        TyKind::Ref(_, _, Mutability::Mut) => 1,
                        _ => 0,
                    };
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    // Missing field-slot shadow on projected carrier refs should be repaired
                    // by rebuilding a ref tag from the loaded pointer value, not by copying the
                    // outer carrier anchor into the local tag directly.
                    alias_flags |= 0b10;
                    let repair_ref_func = Operand::function_handle(
                        tcx,
                        hooks.def_id_ref,
                        std::iter::empty(),
                        source_info.span,
                    );
                    let repair_ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(repair_addr_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, is_mut_u8),
                            span: source_info.span,
                        },
                        Spanned {
                            node: Operand::Copy(Place::from(anchor_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, alias_flags),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    let repair_bd = &mut body.basic_blocks_mut()[repair_call_block];
                    if let Some(stmt) = repair_addr_stmt1_opt {
                        repair_bd.statements.push(stmt);
                    }
                    repair_bd.statements.push(repair_addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        repair_bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        repair_bd.statements.append(&mut align_stmts);
                    }
                    repair_bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: repair_ref_func,
                            args: repair_ref_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(repair_cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });

                    body.basic_blocks_mut()[post_load_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tag_is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ));
                    body.basic_blocks_mut()[post_load_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::SwitchInt {
                            discr: Operand::Copy(Place::from(tag_is_zero_local)),
                            targets: SwitchTargets::static_if(
                                0,
                                validate_target,
                                repair_call_block,
                            ),
                        },
                    });
                }

                if let Some(validate_block) = validate_block {
                    let validate_func = Operand::function_handle(
                        tcx,
                        if require_tag {
                            hooks.def_id_require_loaded_ptr_tag
                        } else {
                            hooks.def_id_validate_loaded_ref_tag
                        },
                        std::iter::empty(),
                        source_info.span,
                    );
                    let validate_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    body.basic_blocks_mut()[validate_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: validate_func,
                            args: validate_args,
                            destination: Place::from(tmp_unit),
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
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(slot_addr_stmt1);
                    bd.statements.push(slot_addr_stmt2);
                    if let Some(ptr_addr_stmt1) = ptr_addr_stmt1_opt {
                        bd.statements.push(ptr_addr_stmt1);
                    }
                    bd.statements.push(ptr_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: tag_func,
                            args: tag_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(ref_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopySlot { src_place } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_slot,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopyRange { src_place, size_op } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let (arg_size, mut size_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &size_op);
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_range,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_size,
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.statements.append(&mut size_stmts);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowStoreBoxPointee {
                box_local,
                src_local,
            } = creation_kind.clone()
            {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self
                    .box_pointee_slot_addr_stmts_for_local(
                        tcx,
                        body,
                        source_info,
                        box_local,
                        dst_addr_local,
                    )
                else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let tag_op: Operand<'tcx> =
                    if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let ref_ancestor_op: Operand<'tcx> =
                    if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let export_parent_op: Operand<'tcx> =
                    if let Some(tl) = export_parent_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        tag_op.clone()
                    };
                let recovered_op: Operand<'tcx> = if let Some(tl) =
                    export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                {
                    Operand::Copy(Place::from(*tl))
                } else {
                    self.const_u8(tcx, source_info.span, 0)
                };
                let store_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_store_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let store_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: ref_ancestor_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: export_parent_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: recovered_op,
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

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: store_func,
                            args: store_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            // workaround for pointers produced from NonNull/Unique via Transmute
            // RawRoot lowering: we implement this by mirroring the existing Raw lowering code path:
            //   tag(ptr_local) = __record_raw_ptr_creation(expose(ptr_local), is_mut, 0)
            if let InstrKind::RawRoot {
                ptr_local,
                is_mut,
                exposed_provenance,
            } = creation_kind.clone()
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if !self.is_pointer_ty(ptr_ty) {
                    continue;
                }
                let raw_root_is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
                let dst_tag = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RawRoot");

                // Compute exposed address first. This is fallible for some pointer shapes, and we must
                // not mutate CFG until we know RawRoot lowering can be emitted completely.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let Some((data_ptr_stmt_opt, addr_stmt)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(ptr_local),
                    addr_local,
                ) else {
                    continue;
                };
                let is_mut_u8: u8 = if is_mut { 1 } else { 0 };
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
                if !bounds_len_stmts.is_empty() {
                    // Appended after address statements once call_bb exists.
                }

                // We insert using the same “split block with a call terminator” style used elsewhere.
                // Create fresh block that will run the call and then continue.
                let is_cleanup = body.basic_blocks[bb].is_cleanup;

                // Split the current block at the correct position so RawRoot runs
                // before or after the target statement based on insert_before.
                let mut tail_stmts: Vec<Statement<'tcx>> = Vec::new();
                {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
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
                let call_bb = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto { target: call_bb },
                });

                if let Some(data_ptr_stmt) = data_ptr_stmt_opt {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .push(data_ptr_stmt);
                }
                body.basic_blocks_mut()[call_bb].statements.push(addr_stmt);
                if !bounds_len_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut bounds_len_stmts);
                }
                if !align_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut align_stmts);
                }

                let root_func = Operand::function_handle(
                    tcx,
                    if raw_root_is_ref {
                        hooks.def_id_ref
                    } else {
                        hooks.def_id_raw
                    },
                    std::iter::empty(),
                    source_info.span,
                );
                let args_root: Box<[Spanned<Operand<'tcx>>]> = vec![
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
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            (if alias_exempt { 1 } else { 0 })
                                | (if exposed_provenance { 0b10_0000 } else { 0 }),
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[call_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: root_func,
                        args: args_root,
                        destination: Place::from(dst_tag),
                        target: Some(cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Normal,
                        fn_span: source_info.span,
                    },
                });

                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    body.basic_blocks_mut()[cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(if raw_root_is_ref {
                                    Operand::Copy(Place::from(dst_tag))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }),
                            ))),
                        ),
                    );
                }

                // Done handling this insert point.
                continue;
            }

            // Caller-side: take return tag after a call returned a pointer into `dst_local`.
            //
            // IMPORTANT: this must be per-call-edge, not per-target-block.
            // A single target block may have multiple call predecessors. If we patch the
            // target block in-place, we would incorrectly run one call's RetTake logic for
            // all predecessors and corrupt tag lineage.
            //
            // We therefore create:
            //   call_bb -> ret_take_bb -> ret_take_cont_bb -> orig_target
            // and only rewrite this call's `target` to `ret_take_bb`.
            if let InstrKind::MutArgRetTake {
                callee_id,
                arg_index,
                local,
                ptr_local,
            } = creation_kind
            {
                let Some(slot_state) = self.carrier_slot_locals_for_local(
                    local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("MutArgRetTake on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                }

                body.basic_blocks_mut()[ret_take_cont_bb].statements.splice(
                    0..0,
                    [
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_u64_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(is_zero_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(nonzero_u64_local),
                                Rvalue::BinaryOp(
                                    BinOp::Sub,
                                    Box::new((
                                        self.const_u64(tcx, source_info.span, 1),
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(keep_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                        Operand::Copy(Place::from(slot_state.anchor_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(new_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(nonzero_u64_local)),
                                        Operand::Copy(Place::from(dst_tag_local)),
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
                                        Operand::Copy(Place::from(keep_part_local)),
                                        Operand::Copy(Place::from(new_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(slot_state.anchor_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ),
                    ],
                );
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        7,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            if let InstrKind::RetTake {
                callee_id,
                dst_local,
            } = creation_kind
            {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing tag local for RetTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                // Compute the address from the return place. For wide pointers we extract the
                // data pointer first so the tag maps to the same address used by raw reads.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(dst_local),
                        addr_local,
                    )
                    .expect("RetTake on non-pointer local");

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_ret_tag_or_root,
                    std::iter::empty(),
                    source_info.span,
                );

                let dst_ty = body.local_decls[dst_local].ty;
                let is_mut = match dst_ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                        // Call-return ref retagging can lose the caller-side parent tag and
                        // materialize a fresh root at the same stack address. Mark these for
                        // runtime same-address lineage repair.
                        flags |= 0b10;
                    }
                    flags
                };
                let bounds_len_op = if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, dst_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 }),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        take_bd.statements.push(addr_stmt1);
                    }
                    take_bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        take_bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                // Redirect only this call edge to the RetTake trampoline block.
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                }

                // Keep ref-ancestor in sync for return-tag recovery.
                // Without this, later PtrDerive on the returned pointer may pick an
                // uninitialized ref-ancestor local (0) and lose provenance, causing
                // OOB accesses to degrade into WILD_POINTER.
                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&dst_local)
                    .copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetAnchorTake { callee_id, local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_tag,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetLeafTake {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            if let InstrKind::RetAnchorRoot { local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorRoot");
                let local_ty = body.local_decls[local].ty;
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorRoot");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("RetAnchorRoot on unsupported local");
                let align_op = self.align_operand_for_ty(tcx, body, local_ty, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_raw,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
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
                            Spanned {
                                node: self.const_u8(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: arg_align,
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::MutArgRetLeafTake {
                callee_id,
                arg_index,
                local,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (base_addr_stmt1, base_addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        base_addr_local,
                        false,
                    )
                    .expect("MutArgRetLeafTake on unsupported local");
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_mut_arg_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(base_addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(slot_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(base_addr_stmt1);
                    take_bd.statements.push(base_addr_stmt2);
                    take_bd.statements.push(slot_addr_stmt1);
                    take_bd.statements.push(slot_addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            // Caller-side pointer-only writeback:
            // forwarders that receive `&mut T` directly do not have a separate carrier local in
            // this frame, but the live pointer local still needs the post-call family before any
            // later use or re-export.
            if let InstrKind::MutArgRetTakePtrOnly {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTakePtrOnly");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("MutArgRetTakePtrOnly on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag_or_zero,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        take_bd.statements.push(addr_stmt1);
                    }
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Eq,
                                        Box::new((
                                            Operand::Copy(Place::from(dst_tag_local)),
                                            self.const_u64(tcx, source_info.span, 0),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_u64_local),
                                    Rvalue::Cast(
                                        CastKind::IntToInt,
                                        Operand::Copy(Place::from(is_zero_local)),
                                        tcx.types.u64,
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(nonzero_u64_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Sub,
                                        Box::new((
                                            self.const_u64(tcx, source_info.span, 1),
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_new_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(nonzero_u64_local)),
                                            Operand::Copy(Place::from(dst_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(ptr_new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::MutArgRetPush {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for MutArgRetPush");

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
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
                bd.statements.push(addr_stmt1);
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::MutArgRetLeafPush {
                callee_id,
                arg_index,
                ptr_local,
                leaf_key,
            } = creation_kind
            {
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((base_addr_stmt1, base_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    base_addr_local,
                    false,
                ) else {
                    continue;
                };
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(base_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(slot_addr_local)),
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
                bd.statements.push(base_addr_stmt1);
                bd.statements.push(base_addr_stmt2);
                bd.statements.push(slot_addr_stmt1);
                bd.statements.push(slot_addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            // Callee-side: push the return anchor for a by-value direct-ref carrier immediately
            // before the `Return` terminator.
            if let InstrKind::RetAnchorPush { callee_id, local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorPush");
                let borrow_kind = self
                    .first_direct_ref_field_place(tcx, body, local)
                    .map(|(_, _, is_mut)| {
                        if is_mut {
                            BorrowKind::Mut {
                                kind: MutBorrowKind::Default,
                            }
                        } else {
                            BorrowKind::Shared
                        }
                    })
                    .unwrap_or(BorrowKind::Shared);
                let mut anchor_select_stmts = Vec::new();
                let selected_anchor_local = self
                    .backtrack_unique_aggregate_anchor_source_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        local,
                        reborrow_anchor_local_for_stack_local,
                    )
                    .and_then(|src_local| {
                        let src_ty = body.local_decls[src_local].ty;
                        let src_tag_local = tag_local_for_ptr_local.get(&src_local).copied()?;
                        if !matches!(src_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
                            return None;
                        }
                        Some(self.materialize_boundary_recovered_source_tag_local(
                            tcx,
                            body,
                            source_info,
                            src_local,
                            src_tag_local,
                            export_parent_local_for_ptr_local,
                            export_parent_is_recovered_local_for_ptr_local,
                            &mut anchor_select_stmts,
                        ))
                    })
                    .or_else(|| {
                        self.materialize_projectionless_slot_anchor_parent_local(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            Place::from(local),
                            borrow_kind,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            anchor_is_slot_family_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            &mut anchor_select_stmts,
                        )
                    })
                    .unwrap_or(slot_state.anchor_local);

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
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(selected_anchor_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    bd.statements.extend(anchor_select_stmts);
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

                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::RetPush {
                callee_id,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RetPush");

                // Use the data pointer for wide return values so tag passing stays consistent.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                // Use the same address extraction helper for wide pointers.
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("RetPush on non-pointer local");

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let selected_tag_local = if let (Some(export_parent_local), Some(recovered_local)) = (
                    export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied(),
                ) {
                    let recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let not_recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let fallback_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let export_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let selected_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let bd = &mut body.basic_blocks_mut()[bb];
                    bd.statements.extend([
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
                                Place::from(fallback_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(not_recovered_u64_local)),
                                        Operand::Copy(Place::from(tag_local)),
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
                                        Operand::Copy(Place::from(fallback_part_local)),
                                        Operand::Copy(Place::from(export_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                    ]);
                    Some(selected_local)
                } else {
                    None
                };

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
                        node: Operand::Copy(Place::from(selected_tag_local.unwrap_or(tag_local))),
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
                if let Some(addr_stmt1) = addr_stmt1_opt {
                    bd.statements.push(addr_stmt1);
                }
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::RetValidate { callee_id, local } = creation_kind {
                let tag_op = self.parent_tag_operand_for_src_place(
                    tcx,
                    body,
                    bb,
                    stmt_idx,
                    source_info,
                    Place::from(local),
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    reborrow_anchor_local_for_stack_local,
                    projectionless_anchor_suppressed_locals,
                    false,
                    true,
                    ParentSelectionMode::PointeeFamily,
                );
                let validate_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_validate_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_validate: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
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
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: validate_func,
                        args: args_validate,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
                continue;
            }

            if let InstrKind::RetLeafPush {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
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
                bd.statements.push(addr_stmt1);
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagProp {
                dst,
                src,
                copy_tag,
                copy_ref_ancestor,
            } = creation_kind
            {
                let prop_stmt = if copy_tag {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for TagProp dst");

                    let src_op: Operand<'tcx> =
                        if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*src_tag))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };

                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_tag),
                            Rvalue::Use(src_op),
                        ))),
                    ))
                } else {
                    None
                };

                let prop_ref_ancestor_stmt = if copy_ref_ancestor {
                    let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for TagProp dst");
                    let src_ref_ancestor_op: Operand<'tcx> = if let Some(src_ref_ancestor) =
                        ref_ancestor_local_for_ptr_local.get(&src)
                    {
                        Operand::Copy(Place::from(*src_ref_ancestor))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_ref_ancestor),
                            Rvalue::Use(src_ref_ancestor_op),
                        ))),
                    ))
                } else {
                    None
                };

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut next_insert = insert_at;
                if let Some(prop_stmt) = prop_stmt {
                    bd.statements.insert(next_insert, prop_stmt);
                    next_insert += 1;
                }
                if let Some(prop_ref_ancestor_stmt) = prop_ref_ancestor_stmt {
                    bd.statements.insert(next_insert, prop_ref_ancestor_stmt);
                    next_insert += 1;
                }
                if copy_tag {
                    if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                        export_parent_local_for_ptr_local.get(&dst).copied(),
                        export_parent_local_for_ptr_local.get(&src).copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        src_export_parent_local,
                                    ))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                    if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&dst)
                            .copied(),
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&src)
                            .copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                }
                continue;
            }

            if let InstrKind::TagPropFromRefAncestor { dst, src } = creation_kind {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst)
                    .expect("missing tag local for TagPropFromRefAncestor dst");
                let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                    .get(&dst)
                    .expect("missing ref-ancestor local for TagPropFromRefAncestor dst");
                let src_ref_ancestor_op: Operand<'tcx> =
                    if let Some(src_ref_ancestor) = ref_ancestor_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*src_ref_ancestor))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                let tag_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_tag),
                        Rvalue::Use(src_ref_ancestor_op.clone()),
                    ))),
                );
                let ref_ancestor_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_ref_ancestor),
                        Rvalue::Use(src_ref_ancestor_op),
                    ))),
                );

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut prop_stmts = vec![tag_stmt, ref_ancestor_stmt];
                if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                    export_parent_local_for_ptr_local.get(&dst).copied(),
                    export_parent_local_for_ptr_local.get(&src).copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_export_parent_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_export_parent_local))),
                        ))),
                    ));
                }
                if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&dst)
                        .copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&src)
                        .copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_recovered_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, prop_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorZero {
                anchor_local,
                anchor_state_local,
            } = creation_kind
            {
                let mut zero_stmts = vec![Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                    ))),
                )];
                if let Some(anchor_state_local) = anchor_state_local {
                    zero_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.splice(insert_at..insert_at, zero_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSet {
                dst_local: _dst_local,
                anchor_local,
                anchor_state_local,
                src_ptr_local,
            } = creation_kind
            {
                let src_tag_operand = if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_ptr_local).copied()
                {
                    Operand::Copy(Place::from(src_tag_local))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_ptr_local)
                    .copied()
                {
                    Operand::Copy(Place::from(src_anchor_local))
                } else {
                    panic!("missing lineage source for ReborrowAnchorSet src");
                };
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let anchor_is_zero_stmt = Statement::new(
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
                );
                let anchor_should_init_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_should_init_local),
                        Rvalue::Cast(
                            CastKind::IntToInt,
                            Operand::Copy(Place::from(anchor_is_zero_local)),
                            tcx.types.u64,
                        ),
                    ))),
                );
                let anchor_new_part_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_new_part_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(anchor_should_init_local)),
                                src_tag_operand,
                            )),
                        ),
                    ))),
                );
                let anchor_selected_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_selected_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(anchor_local)),
                                Operand::Copy(Place::from(anchor_new_part_local)),
                            )),
                        ),
                    ))),
                );
                let set_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut stmts = vec![
                    anchor_is_zero_stmt,
                    anchor_should_init_stmt,
                    anchor_new_part_stmt,
                    anchor_selected_stmt,
                    set_stmt,
                ];
                if let Some(anchor_state_local) = anchor_state_local {
                    stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSeed {
                dst_local,
                src_local,
                mark_slot_family,
            } = creation_kind
            {
                let mut src_tag_select_stmts = Vec::new();
                let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&dst_local)
                    .copied()
                else {
                    continue;
                };
                let Some(src_tag_operand) = (if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_local).copied()
                {
                    let selected_src_tag_local = self
                        .materialize_boundary_recovered_source_tag_local(
                            tcx,
                            body,
                            source_info,
                            src_local,
                            src_tag_local,
                            export_parent_local_for_ptr_local,
                            export_parent_is_recovered_local_for_ptr_local,
                            &mut src_tag_select_stmts,
                        );
                    Some(Operand::Copy(Place::from(selected_src_tag_local)))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_local)
                    .copied()
                {
                    Some(Operand::Copy(Place::from(src_anchor_local)))
                } else {
                    None
                }) else {
                    continue;
                };
                let dst_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&dst_local)
                    .copied();
                let src_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&src_local)
                    .copied();
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let inserted_stmt_count = src_tag_select_stmts.len() + 5;
                let seed_stmts = [
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
                            Place::from(anchor_should_init_local),
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
                            Place::from(anchor_new_part_local),
                            Rvalue::BinaryOp(
                                BinOp::Mul,
                                Box::new((
                                    Operand::Copy(Place::from(anchor_should_init_local)),
                                    src_tag_operand,
                                )),
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_selected_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(anchor_local)),
                                    Operand::Copy(Place::from(anchor_new_part_local)),
                                )),
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_local),
                            Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                        ))),
                    ),
                ];
                bd.statements.splice(
                    insert_at..insert_at,
                    src_tag_select_stmts.into_iter().chain(seed_stmts),
                );
                if let Some(dst_anchor_state_local) = dst_anchor_state_local {
                    let state_stmt = if mark_slot_family {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        )
                    } else if let Some(src_anchor_state_local) = src_anchor_state_local {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(Operand::Copy(Place::from(src_anchor_state_local))),
                            ))),
                        )
                    } else {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                            ))),
                        )
                    };
                    bd.statements
                        .insert(insert_at + inserted_stmt_count, state_stmt);
                }
                continue;
            }

            if let InstrKind::ParentTagSnapshot {
                dst_local,
                src,
                is_raw_creation,
            } = creation_kind
            {
                let parent_local = *ref_ancestor_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing ref-ancestor local for ParentTagSnapshot dst");
                let parent_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(parent_local),
                        Rvalue::Use(self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            src,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            is_raw_creation,
                            true,
                            self.creation_parent_selection_mode_for_src_place(
                                tcx,
                                body,
                                src,
                                is_raw_creation,
                                true,
                            ),
                        )),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.insert(insert_at, parent_stmt);
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        // Argument retagging often introduces short-lived receiver borrows at
                        // call boundaries. If source-tag plumbing drops the parent, allow runtime
                        // same-address repair instead of creating a sibling root.
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
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
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        // Shared refs at function entry are boundary-recovered values. Preserve
                        // the incoming boundary parent for later call/return transport.
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, recovered_value)),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&local)
                    .copied();
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("ArgAnchorTake on unsupported local");
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
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
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_call_arg_tag_anchor,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(anchor_local),
                        target: Some(cont_block),
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
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    bd.terminator = Some(take_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                if let Some(anchor_state_local) = anchor_state_local {
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }
                continue;
            }

            if let InstrKind::FnExit { callee_id } = creation_kind {
                let exit_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_exit_fn,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_exit: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: self.const_u64(tcx, source_info.span, callee_id),
                    span: source_info.span,
                }]
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
                        func: exit_func,
                        args: args_exit,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            if matches!(
                creation_kind,
                InstrKind::IndirectCallScopeBegin | InstrKind::IndirectCallScopeEnd
            ) {
                let func_def = match creation_kind {
                    InstrKind::IndirectCallScopeBegin => hooks.def_id_begin_indirect_call_arg_scope,
                    InstrKind::IndirectCallScopeEnd => hooks.def_id_end_indirect_call_arg_scope,
                    _ => unreachable!(),
                };
                let func =
                    Operand::function_handle(tcx, func_def, std::iter::empty(), source_info.span);
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
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func,
                        args: Vec::new().into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
                continue;
            }

            if let InstrKind::TagKill { ptr_local } = creation_kind {
                let Some(tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() else {
                    continue;
                };
                let mut tag_locals: Vec<Local> = vec![tag_local];
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    if export_parent_local != tag_local {
                        tag_locals.push(export_parent_local);
                    }
                }

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let mut next_target = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let cont_block = next_target;
                for kill_local in tag_locals.into_iter().rev() {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let call_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_tag_kill,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![Spanned {
                                node: Operand::Copy(Place::from(kill_local)),
                                span: source_info.span,
                            }]
                            .into_boxed_slice(),
                            destination: Place::from(tmp_unit),
                            target: Some(next_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    next_target = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(Some(call_term), is_cleanup));
                }
                let mut reset_stmts = Vec::new();
                reset_stmts.push(Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(tag_local),
                        Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                    ))),
                ));
                if let Some(ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ref_ancestor_local),
                            Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(export_parent_local),
                            Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(recovered_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, reset_stmts);
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto {
                        target: next_target,
                    },
                });
                continue;
            }

            if let InstrKind::TagLocalKill { tag_local } = creation_kind {
                let kill_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_kill,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_kill: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
                    span: source_info.span,
                }]
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
                        func: kill_func,
                        args: args_kill,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagRetain { tag_local } = creation_kind {
                let retain_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_retain,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_retain: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
                    span: source_info.span,
                }]
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
                        func: retain_func,
                        args: args_retain,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            let mut func_operand =
                self.func_operand_for(tcx, hooks, &creation_kind, source_info.span);
            if let InstrKind::ShadowStore { src_local } = creation_kind {
                if self.shadow_store_uses_local_slot_store(
                    local_slot_shadow_store_locals,
                    place,
                    src_local,
                ) {
                    func_operand = Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_store_ptr_local,
                        std::iter::empty(),
                        source_info.span,
                    );
                }
            }

            let insert_before: bool = ip.insert_before;

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
                InstrKind::RetRoot { dst_local, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst_local)
                        .expect("missing preallocated tag local for RetRoot destination"),
                ),
                InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing preallocated tag local for PtrDerive destination"),
                ),
                _ => None,
            };

            let addr_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));

            // Compute the address for runtime hooks. We route all pointer cases through
            // addr_stmts_for_place so wide pointers are handled via their data pointer.
            let mut addr_extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            let (addr_stmt1_opt, addr_stmt2) = match creation_kind {
                InstrKind::StackAlloc { local, .. } => {
                    let local_ty = body.local_decls[local].ty;
                    // Stack-slot tracking only needs the numeric address of a concrete local in
                    // this MIR body. Opaque/generic-but-sized locals (for example
                    // `impl RangeBounds<usize>` in optimized `bytes::Bytes::slice`) are still
                    // valid `&raw const local` sources even though `is_thin_ptr_ty` rejects them
                    // for generic pointer-value exposure.
                    if !local_ty.is_sized(tcx, body.typing_env(tcx)) {
                        continue;
                    }
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
                // Heap/const allocations already have a pointer local; just expose its address.
                InstrKind::HeapAlloc { ptr_local, .. }
                | InstrKind::ConstAlloc { ptr_local, .. } => {
                    match self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    ) {
                        Some(stmts) => stmts,
                        None => continue,
                    }
                }
                InstrKind::ConstAllocConst { const_op, .. } => {
                    let const_ty = const_op.const_.ty();
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(const_ty, source_info.span));
                    let assign_const = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::Use(Operand::Constant(Box::new(const_op.clone()))),
                        ))),
                    );
                    let Some((opt_stmt, addr_stmt)) = self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(tmp_ptr),
                        addr_local,
                    ) else {
                        continue;
                    };
                    // If we need multiple address statements (wide pointer), skip.
                    if opt_stmt.is_some() {
                        continue;
                    }
                    (Some(assign_const), addr_stmt)
                }
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. } => {
                    if let Some((opt, stmt, offset_stmts)) =
                        self.addr_stmts_for_access_place(tcx, body, source_info, place, addr_local)
                    {
                        addr_extra_stmts = offset_stmts;
                        (opt, stmt)
                    } else {
                        // Fallback for projection-heavy deref accesses where static offset
                        // recovery failed (e.g., generic field layout):
                        // materialize `&raw {const,mut} <full place>` and expose that pointer.
                        // Using only `place.local` here points at the base carrier and can
                        // turn valid projected accesses into false OOB/WILD reports.
                        let place_ty = place.ty(&body.local_decls, tcx).ty;
                        let is_write = matches!(
                            creation_kind,
                            InstrKind::PtrWrite { .. } | InstrKind::PtrWriteAllowUntagged { .. }
                        );
                        let raw_ptr_ty = if is_write {
                            Ty::new_mut_ptr(tcx, place_ty)
                        } else {
                            Ty::new_imm_ptr(tcx, place_ty)
                        };
                        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
                            continue;
                        }
                        let tmp_ptr = body
                            .local_decls
                            .push(LocalDecl::new(raw_ptr_ty, source_info.span));
                        let raw_ptr_stmt = Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tmp_ptr),
                                Rvalue::RawPtr(
                                    if is_write {
                                        RawPtrKind::Mut
                                    } else {
                                        RawPtrKind::Const
                                    },
                                    place,
                                ),
                            ))),
                        );
                        match self.addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(tmp_ptr),
                            addr_local,
                        ) {
                            Some((opt_stmt, addr_stmt)) => {
                                if let Some(s) = opt_stmt {
                                    addr_extra_stmts.push(s);
                                }
                                (Some(raw_ptr_stmt), addr_stmt)
                            }
                            None => continue,
                        }
                    }
                }
                InstrKind::StackSlotWriteAllowUntagged { .. } => {
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        true,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::CallArgPush { ptr_local, .. }
                | InstrKind::IndirectCallArgPush { ptr_local, .. } => {
                    let place_ty = place.ty(&body.local_decls, tcx).ty;
                    if self.is_pointer_ty(place_ty) {
                        match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                            Some(stmts) => stmts,
                            None => continue,
                        }
                    } else {
                        let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            place,
                            addr_local,
                            false,
                        ) else {
                            continue;
                        };
                        (Some(slot_stmt1), slot_stmt2)
                    }
                }
                InstrKind::CallArgLeafPush { .. }
                | InstrKind::IndirectCallArgLeafPush { .. }
                | InstrKind::ArgLeafTake { .. } => {
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        false,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::CallArgValidate { .. } | InstrKind::RetValidate { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. } => {
                    let is_mut = matches!(
                        creation_kind,
                        InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. }
                    );
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        is_mut,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                _ => match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                    Some(stmts) => stmts,
                    None => continue,
                },
            };

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

            let orig_stmt_len = orig_stmt_prefix_len
                .get(&bb)
                .copied()
                .unwrap_or_else(|| body.basic_blocks[bb].statements.len());

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc {
                    ptr_local, live, ..
                } => Some((*ptr_local, *live)),
                _ => None,
            };

            // For const/global allocations, rewrite the exposed pointer address to the base.
            let mut arg_addr_local = addr_local;
            let mut addr_adjust_stmt_opt: Option<Statement<'tcx>> = None;

            if let InstrKind::ConstAlloc { base_offset, .. }
            | InstrKind::ConstAllocConst { base_offset, .. } = &creation_kind
            {
                if *base_offset != 0 {
                    let base_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let offset_op = self.const_usize(tcx, source_info.span, *base_offset);
                    addr_adjust_stmt_opt = Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(base_local),
                            Rvalue::BinaryOp(
                                BinOp::Sub,
                                Box::new((Operand::Copy(Place::from(addr_local)), offset_op)),
                            ),
                        ))),
                    ));
                    arg_addr_local = base_local;
                }
            }

            let arg_addr = Operand::Copy(Place::from(arg_addr_local));

            let mut extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            if !addr_extra_stmts.is_empty() {
                extra_stmts.extend(addr_extra_stmts);
            }
            let projected_ref_parent_local = match &creation_kind {
                InstrKind::Ref {
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } if !src.projection.is_empty() => self
                    .materialize_projected_reborrow_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        projected_reborrow_anchor_key.as_ref(),
                        projected_reborrow_anchor_local_for_key,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    ),
                _ => None,
            };
            let projectionless_ref_parent_local = match &creation_kind {
                InstrKind::Ref { bk, src, .. }
                    if src.projection.is_empty()
                        && projectionless_anchor_suppressed_locals.contains(&src.local) =>
                {
                    self.materialize_projectionless_slot_anchor_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        *bk,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    )
                }
                _ => None,
            };

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrReadAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // Projected accesses like `(*self_ref).field`, `slice[i]`, or
                        // `(*wide_ref).0` must validate in the family of the accessed
                        // projection source, not blindly in the family of the coarse
                        // `ptr_local` chosen during scan. `PtrUse` already does this;
                        // keep read validation consistent to avoid reusing an outer
                        // `&Bytes` tag on the empty-slice sentinel pointer `0x1`.
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackAlloc {
                    ref size_op, live, ..
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    // Encode stack-alloc flag in bit1; bit0 is the live flag.
                    let live_bits: u8 = if live { 1 } else { 0 };
                    let arg_live = self.const_u8(tcx, source_info.span, live_bits | 0x2);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::HeapAlloc {
                    ptr_local: _,
                    live,
                    ref size_op,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                // Record a live global/promoted allocation at the computed base address.
                InstrKind::ConstAlloc { size, .. } | InstrKind::ConstAllocConst { size, .. } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let arg_size = self.const_usize(tcx, source_info.span, size);
                    // __rz_record_alloc live bitfield:
                    // bit0 = live, bit1 = stack, bit2 = global/promoted const
                    let arg_live = self.const_u8(tcx, source_info.span, 0b101);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrWrite {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrWriteAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackSlotWriteAllowUntagged {
                    local, ref size_op, ..
                } => {
                    let Some(tag_local) =
                        reborrow_anchor_local_for_stack_local.get(&local).copied()
                    else {
                        continue;
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(tag_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                ref kind @ (InstrKind::CallArgPush { .. }
                | InstrKind::IndirectCallArgPush { .. }) => {
                    let (callee_id_opt, arg_index, ptr_local, parent_mode, flags) = match kind {
                        InstrKind::CallArgPush {
                            callee_id,
                            arg_index,
                            ptr_local,
                            parent_mode,
                            flags,
                        } => (
                            Some(*callee_id),
                            *arg_index,
                            *ptr_local,
                            *parent_mode,
                            *flags,
                        ),
                        InstrKind::IndirectCallArgPush {
                            arg_index,
                            ptr_local,
                            parent_mode,
                            flags,
                        } => (None, *arg_index, *ptr_local, *parent_mode, *flags),
                        _ => unreachable!(),
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let fallback_tag_op: Operand<'tcx> = if place.projection.is_empty()
                        && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                    {
                        let pointee_anchor_local = self
                            .backtrack_pointer_pointee_local(
                                body,
                                ptr_local,
                                &body.basic_blocks[bb].statements,
                            )
                            .and_then(|pointee_local| {
                                if self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                                    None
                                } else {
                                    reborrow_anchor_local_for_stack_local
                                        .get(&pointee_local)
                                        .copied()
                                }
                            });
                        if let Some(anchor_local) = pointee_anchor_local {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                let tag_is_zero_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                let tag_is_zero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let tag_is_nonzero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let keep_tag_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let anchor_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let selected_tag_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                extra_stmts.extend([
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Eq,
                                                Box::new((
                                                    Operand::Copy(Place::from(tl)),
                                                    self.const_u64(tcx, source_info.span, 0),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_u64_local),
                                            Rvalue::Cast(
                                                CastKind::IntToInt,
                                                Operand::Copy(Place::from(tag_is_zero_local)),
                                                tcx.types.u64,
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_nonzero_u64_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Sub,
                                                Box::new((
                                                    self.const_u64(tcx, source_info.span, 1),
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(keep_tag_part_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Mul,
                                                Box::new((
                                                    Operand::Copy(Place::from(
                                                        tag_is_nonzero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(tl)),
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
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(anchor_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(selected_tag_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Add,
                                                Box::new((
                                                    Operand::Copy(Place::from(keep_tag_part_local)),
                                                    Operand::Copy(Place::from(anchor_part_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                ]);
                                Operand::Copy(Place::from(selected_tag_local))
                            } else {
                                Operand::Copy(Place::from(anchor_local))
                            }
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                            Operand::Copy(Place::from(tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // For projected call arguments such as subslices (`output[a..b]`) or
                        // field projections, pushing the carrier local's current tag is often too
                        // weak: optimized MIR can keep only a wrapper temp tagged while the actual
                        // projected argument never materializes its own stable tag before the call.
                        //
                        // The callee only needs a parent lineage to retag its local argument at
                        // the callee address. Reuse the same parent-selection logic we use for
                        // ref/raw creation so interprocedural retagging stays attached to the
                        // source borrow family instead of falling back to a root inside the callee.
                        if matches!(place.projection.first(), Some(ProjectionElem::Deref))
                            && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                        {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                Operand::Copy(Place::from(tl))
                            } else if let Some(tl) =
                                ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                            {
                                Operand::Copy(Place::from(tl))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    place,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    true,
                                    parent_mode,
                                )
                            }
                        } else {
                            self.parent_tag_operand_for_src_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                source_info,
                                place,
                                tag_local_for_ptr_local,
                                ref_ancestor_local_for_ptr_local,
                                reborrow_anchor_local_for_stack_local,
                                projectionless_anchor_suppressed_locals,
                                false,
                                true,
                                parent_mode,
                            )
                        }
                    };
                    let exact_tag_op = fallback_tag_op;
                    let (boundary_parent_op, boundary_origin_op): (Operand<'tcx>, Operand<'tcx>) =
                        if let (Some(export_parent_local), Some(recovered_local)) = (
                            export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                            export_parent_is_recovered_local_for_ptr_local
                                .get(&ptr_local)
                                .copied(),
                        ) {
                            (
                                Operand::Copy(Place::from(export_parent_local)),
                                Operand::Copy(Place::from(recovered_local)),
                            )
                        } else {
                            (
                                exact_tag_op.clone(),
                                self.const_u8(
                                    tcx,
                                    source_info.span,
                                    CALL_ARG_BOUNDARY_ORIGIN_EXACT,
                                ),
                            )
                        };

                    let mut arg_vec: Vec<Spanned<Operand<'tcx>>> = Vec::new();
                    if let Some(callee_id) = callee_id_opt {
                        arg_vec.push(Spanned {
                            node: self.const_u64(tcx, source_info.span, callee_id),
                            span: source_info.span,
                        });
                    }
                    arg_vec.extend([
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, arg_index),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: exact_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: boundary_parent_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: boundary_origin_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, flags),
                            span: source_info.span,
                        },
                    ]);

                    let args: Box<[Spanned<Operand<'tcx>>]> = arg_vec.into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgValidate { local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op = self.parent_tag_operand_for_src_place(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        place,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        false,
                        true,
                        ParentSelectionMode::PointeeFamily,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();

                    let _ = local;
                    (args, Place::from(tmp_unit))
                }

                ref kind @ (InstrKind::CallArgLeafPush { .. }
                | InstrKind::IndirectCallArgLeafPush { .. }
                | InstrKind::ArgLeafTake { .. }) => {
                    let (callee_id_opt, arg_index, leaf_key) = match kind {
                        InstrKind::CallArgLeafPush {
                            callee_id,
                            arg_index,
                            leaf_key,
                        }
                        | InstrKind::ArgLeafTake {
                            callee_id,
                            arg_index,
                            leaf_key,
                        } => (Some(*callee_id), *arg_index, *leaf_key),
                        InstrKind::IndirectCallArgLeafPush {
                            arg_index,
                            leaf_key,
                        } => (None, *arg_index, *leaf_key),
                        _ => unreachable!(),
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let mut arg_vec: Vec<Spanned<Operand<'tcx>>> = Vec::new();
                    if let Some(callee_id) = callee_id_opt {
                        arg_vec.push(Spanned {
                            node: self.const_u64(tcx, source_info.span, callee_id),
                            span: source_info.span,
                        });
                    }
                    arg_vec.extend([
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, arg_index),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, leaf_key),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                    ]);
                    let args: Box<[Spanned<Operand<'tcx>>]> = arg_vec.into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrUse { ptr_local } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            ParentSelectionMode::PointeeFamily,
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::DebugRefActivate {
                    raw_local,
                    tag_local,
                    is_mut,
                } => {
                    let raw_tag_op = if let Some(tl) = tag_local_for_ptr_local.get(&raw_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                    let ref_ancestor_op =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&raw_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let alias_exempt =
                        self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[raw_local].ty);
                    let arg_alias =
                        self.const_u8(tcx, source_info.span, if alias_exempt { 1 } else { 0 });
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        raw_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, raw_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: raw_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tag_local))
                }

                InstrKind::ShadowStore { src_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let ref_ancestor_op: Operand<'tcx> =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let export_parent_op: Operand<'tcx> =
                        if let Some(tl) = export_parent_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            tag_op.clone()
                        };
                    let recovered_op: Operand<'tcx> = if let Some(tl) =
                        export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                    {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u8(tcx, source_info.span, 0)
                    };
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: export_parent_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: recovered_op,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::ShadowKill { ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagKill { ptr_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op = tag_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                        .map(|tag_local| Operand::Copy(Place::from(tag_local)))
                        .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagLocalKill { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagRetain { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrDerive {
                    dst,
                    src,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDerive dst");

                    let parent_from_src: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*tl))
                        } else if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };

                    let parent_tag_op: Operand<'tcx> = match &creation_kind {
                        // For derivations, use the source local's current tag when available.
                        // Falling back to ref-ancestor is only a backup path.
                        InstrKind::PtrDerive { is_ref: true, .. } => parent_from_src,
                        _ => {
                            // Keep raw derivations connected to source provenance.
                            parent_from_src
                        }
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    // Bitfield semantics match __record_* hooks:
                    // bit0=alias_exempt, bit1=basic lineage-repair hint, bit2=strong hint,
                    // bit3=carry wide bounds from src when derivation drops metadata.
                    // Raw PtrDerive in optimized MIR often comes from projection-heavy lowering
                    // and benefits from runtime parent repair when stack metadata is coarse.
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if self.should_forward_bounds_from_src_ptr_derive(tcx, body, src, dst) {
                            alias_flags |= 0b1000;
                        }
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = self.align_operand_for_ptr_derive(
                        tcx,
                        body,
                        src,
                        dst,
                        is_ref,
                        source_info.span,
                    );
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                InstrKind::PtrDeriveParent {
                    dst,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDeriveParent dst");
                    let parent_local = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for PtrDeriveParent dst");

                    let parent_tag_op: Operand<'tcx> = Operand::Copy(Place::from(parent_local));

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                _ => {
                    let is_mut_u8: u8 = match creation_kind {
                        InstrKind::Ref {
                            bk: borrow_kind, ..
                        } => match borrow_kind {
                            BorrowKind::Mut { .. } => 1,
                            _ => 0,
                        },
                        InstrKind::Raw { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        InstrKind::RetRoot { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        _ => 0,
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, is_mut_u8);

                    let arg_parent: Operand<'tcx> = match &creation_kind {
                        InstrKind::Ref { bk, src, .. } => {
                            let use_projectionless_anchor = matches!(bk, BorrowKind::Mut { .. })
                                || self.compile_alias_model_is_sb_like();
                            if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        false,
                                        use_projectionless_anchor,
                                    ),
                                )
                            }
                        }
                        InstrKind::Raw { src, .. } => {
                            let src_local = src.local;
                            let src_is_plain_ref_deref = src.projection.len() == 1
                                && matches!(src.projection[0], ProjectionElem::Deref)
                                && matches!(body.local_decls[src_local].ty.kind(), TyKind::Ref(..));
                            if src_is_plain_ref_deref {
                                if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                                    Operand::Copy(Place::from(*tl))
                                } else if let Some(tl) =
                                    ref_ancestor_local_for_ptr_local.get(&src_local)
                                {
                                    Operand::Copy(Place::from(*tl))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    true,
                                    true,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx, body, *src, true, true,
                                    ),
                                )
                            }
                        }
                        _ => self.const_u64(tcx, source_info.span, 0),
                    };

                    let alias_exempt = match &creation_kind {
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            if let Some(dst_local) = place.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                self.alias_exempt_for_ptr_ty(tcx, body, dst_ty)
                                    || self.alias_exempt_child_from_source_ptr(
                                        tcx,
                                        body,
                                        src.ty(&body.local_decls, tcx).ty,
                                        dst_ty,
                                    )
                            } else {
                                let ty = src.ty(&body.local_decls, tcx).ty;
                                self.alias_exempt_for_ty(tcx, body, ty)
                            }
                        }
                        InstrKind::RawRoot { ptr_local, .. } => {
                            let ty = body.local_decls[*ptr_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let ty = body.local_decls[*dst_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        _ => false,
                    };
                    // `alias_exempt` argument is a bitfield:
                    // - bit0: alias-exempt pointee classification (existing behavior)
                    // - bit1: projected-source creation hint (used by runtime lineage repair)
                    // - bit2: stronger root-origin repair hint (bounded overlap recovery)
                    // - bit4: TB-lite raw is a derived same-family view
                    // - bit6: raw creation should validate projected/derived provenance immediately
                    // - bit7: deref-based raw creation must reject exposed/no-provenance parents
                    let alias_flags: u8 = match &creation_kind {
                        InstrKind::Ref { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Even simple reference reborrows can lose their parent tag in
                            // optimized MIR/call-boundary lowering and show up as fresh roots at
                            // the same stack address. Always allow exact same-address repair for
                            // refs; keep the stronger bounded-overlap recovery limited to
                            // projection-heavy sources.
                            flags |= 0b10;
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                            }
                            let src_ty = src.ty(&body.local_decls, tcx).ty;
                            if self.is_pointer_ty(src_ty) && !self.is_thin_ptr_ty(tcx, body, src_ty)
                            {
                                // Wide-pointer reborrows often lower through a temporary thin raw
                                // data pointer. If that helper raw root loses lineage, the runtime
                                // should prefer dropping the bad raw-root parent over freezing the
                                // eventual wide ref/write as a foreign sibling.
                                flags |= 0b1_0000;
                            }
                            if src.projection.is_empty()
                                && projectionless_anchor_suppressed_locals.contains(&src.local)
                            {
                                // Whole-slot reborrows of by-value return carriers (`other:
                                // BytesMut; &mut other`) need targeted same-slot root retirement
                                // in TB-lite. Mark only this path so ordinary repeated `&mut`
                                // call arguments do not invalidate each other.
                                flags |= 0b1000;
                            }
                            flags
                        }
                        InstrKind::Raw { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Raw creation can still lose lineage when source-tag plumbing is
                            // missing (e.g. wrapper/projection-heavy optimized MIR). Mark all
                            // raw creations as eligible for runtime best-effort repair.
                            flags |= 0b10;
                            // Tree Borrows treats raw pointer creation as tag-preserving:
                            // casts, projections, and pointer arithmetic transport the source
                            // family rather than allocating a new permission-bearing borrow node.
                            // `InstrKind::Raw` is the structurally derived case; true fresh or
                            // unknown roots are emitted as `RawRoot`/`RetRoot` below. Mark every
                            // derived raw view so TB-lite keeps it as tag-store metadata until an
                            // actual raw write needs access-local raw state.
                            flags |= 0b0001_0000;
                            // For projected raw sources (`(*p).field`, etc.) also allow strong
                            // bounded-overlap parent recovery in the runtime repair path.
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                                flags |= 0b0100_0000;
                            }
                            if matches!(src.projection.first(), Some(ProjectionElem::Deref))
                                && !self
                                    .raw_creation_allows_no_provenance_transport(tcx, body, *src)
                            {
                                flags |= 0b1000_0000;
                            }
                            flags
                        }
                        // RawRoot is emitted exactly in cases where provenance source recovery
                        // failed at instrumentation time. Mark for runtime best-effort repair.
                        InstrKind::RawRoot { .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            flags |= 0b10;
                            flags
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Return-root creation means caller-side provenance recovery failed.
                            // Mark both refs and raws as eligible for exact same-address repair:
                            // uninstrumented std/core pointer-returning wrappers can otherwise
                            // synthesize a fresh root for a pointer that should remain attached to
                            // an existing live lineage at the same address.
                            flags |= 0b10;
                            flags
                        }
                        _ => {
                            if alias_exempt {
                                1
                            } else {
                                0
                            }
                        }
                    };
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);

                    let bounds_ptr_local = match &creation_kind {
                        InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                        InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                        InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                        _ => None,
                    };
                    let bounds_len_op = bounds_ptr_local
                        .map(|pl| match &creation_kind {
                            InstrKind::Ref { .. } => self
                                .ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                ),
                            InstrKind::RetRoot { .. }
                                if matches!(body.local_decls[pl].ty.kind(), TyKind::Ref(..)) =>
                            {
                                self.ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                )
                            }
                            _ => self.bounds_len_operand_for_ptr_local(
                                tcx,
                                body,
                                pl,
                                source_info.span,
                            ),
                        })
                        .unwrap_or_else(|| self.unknown_bounds_len_operand(tcx, source_info.span));
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = match &creation_kind {
                        InstrKind::Ref { src, .. } => self
                            .align_operand_for_ref_creation_src_place(
                                tcx,
                                body,
                                place.ty(&body.local_decls, tcx).ty,
                                *src,
                                source_info.span,
                            ),
                        InstrKind::Raw { src, .. } => {
                            self.align_operand_for_src_place(tcx, body, *src, source_info.span)
                        }
                        _ => bounds_ptr_local
                            .map(|pl| {
                                self.align_operand_for_ptr_local(tcx, body, pl, source_info.span)
                            })
                            .unwrap_or_else(|| {
                                SizeOperand::Const(self.const_usize(tcx, source_info.span, 0))
                            }),
                    };
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let mut explicit_extent: Option<(Operand<'tcx>, Operand<'tcx>)> = None;
                    if let InstrKind::Ref { bk, src, .. } = &creation_kind {
                        if !matches!(bk, BorrowKind::Mut { .. }) {
                            if let Some((arg_extent_base, extent_len_op, mut extent_stmts)) = self
                                .interior_mut_array_extent_for_source_place(
                                    tcx,
                                    body,
                                    source_info,
                                    *src,
                                )
                            {
                                extra_stmts.append(&mut extent_stmts);
                                let (arg_extent_len, mut extent_len_stmts) = self
                                    .materialize_size_operand(
                                        tcx,
                                        body,
                                        source_info,
                                        &extent_len_op,
                                    );
                                extra_stmts.append(&mut extent_len_stmts);
                                func_operand = Operand::function_handle(
                                    tcx,
                                    hooks.def_id_ref_with_extent,
                                    std::iter::empty(),
                                    source_info.span,
                                );
                                explicit_extent = Some((arg_extent_base, arg_extent_len));
                            }
                        }
                    }

                    let mut args = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_parent,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ];
                    if let Some((arg_extent_base, arg_extent_len)) = explicit_extent {
                        args.push(Spanned {
                            node: arg_extent_base,
                            span: source_info.span,
                        });
                        args.push(Spanned {
                            node: arg_extent_len,
                            span: source_info.span,
                        });
                    }
                    let args: Box<[Spanned<Operand<'tcx>>]> = args.into_boxed_slice();

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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);

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
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            if alias_exempt { 1 } else { 0 },
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
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

            let ref_ancestor_init_stmt_opt: Option<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let use_projectionless_anchor = match &creation_kind {
                                InstrKind::Ref { bk, .. } => {
                                    matches!(bk, BorrowKind::Mut { .. })
                                        || self.compile_alias_model_is_sb_like()
                                }
                                _ => true,
                            };
                            let parent_op = if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        false,
                                        use_projectionless_anchor,
                                    ),
                                )
                            };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(parent_op),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(&src.local)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::RawRoot { ptr_local, .. } => ref_ancestor_local_for_ptr_local
                    .get(ptr_local)
                    .copied()
                    .map(|dst_ref_ancestor_local| {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        )
                    }),
                InstrKind::RetRoot {
                    dst_local, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst_local).copied()
                    {
                        if *is_ref {
                            tag_local_for_ptr_local
                                .get(dst_local)
                                .copied()
                                .map(|dst_tag_local| {
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(dst_ref_ancestor_local),
                                            Rvalue::Use(Operand::Copy(Place::from(dst_tag_local))),
                                        ))),
                                    )
                                })
                        } else {
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                InstrKind::PtrDerive {
                    dst, src, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst).copied()
                    {
                        if *is_ref {
                            let src_parent_op: Operand<'tcx> =
                                if let Some(src_tag_local) = tag_local_for_ptr_local.get(src) {
                                    Operand::Copy(Place::from(*src_tag_local))
                                } else if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(src)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_parent_op),
                                ))),
                            ))
                        } else {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(src)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let export_parent_init_stmts: Vec<Statement<'tcx>> = {
                let direct_dst_local = match &creation_kind {
                    InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                    InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                    InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                    InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => {
                        Some(*dst)
                    }
                    InstrKind::DebugRefActivate { raw_local, .. } => Some(*raw_local),
                    _ => None,
                };
                if let Some(dst_local) = direct_dst_local {
                    let mut stmts = Vec::new();
                    let mut recovered_init_local: Option<Local> = None;
                    if let (Some(export_parent_local), Some(dst_tag_local)) = (
                        export_parent_local_for_ptr_local.get(&dst_local).copied(),
                        tag_local_for_ptr_local.get(&dst_local).copied(),
                    ) {
                        let default_export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref {
                                bk,
                                src,
                                projected_reborrow_anchor_key,
                                ..
                            } if matches!(bk, BorrowKind::Mut { .. })
                                && matches!(
                                    body.local_decls[dst_local].ty.kind(),
                                    TyKind::Ref(_, pointee_ty, Mutability::Mut)
                                        if !self.is_pointer_ty(*pointee_ty)
                                ) =>
                            {
                                projected_reborrow_anchor_key
                                    .as_ref()
                                    .and_then(|key| {
                                        projected_reborrow_anchor_local_for_key.get(key).copied()
                                    })
                                    .or_else(|| {
                                        if src.projection.is_empty() {
                                            reborrow_anchor_local_for_stack_local
                                                .get(&src.local)
                                                .copied()
                                        } else {
                                            None
                                        }
                                    })
                                    .map(|anchor_local| Operand::Copy(Place::from(anchor_local)))
                                    .unwrap_or_else(|| Operand::Copy(Place::from(dst_tag_local)))
                            }
                            _ => Operand::Copy(Place::from(dst_tag_local)),
                        };
                        let export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
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
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::Raw { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
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
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::PtrDerive {
                                src, is_ref: true, ..
                            } => {
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(src).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(src)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
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
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
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
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            _ => default_export_parent_op,
                        };
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_op),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&dst_local)
                        .copied()
                    {
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(
                                    recovered_init_local
                                        .map(|local| Operand::Copy(Place::from(local)))
                                        .unwrap_or_else(|| self.const_u8(tcx, source_info.span, 0)),
                                ),
                            ))),
                        ));
                    }
                    stmts
                } else {
                    Vec::new()
                }
            };
            let reborrow_anchor_set_stmts: Vec<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref {
                    bk,
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } => {
                    if matches!(bk, BorrowKind::Mut { .. }) {
                        if let Some(dst_local) = place.as_local() {
                            let anchor_local_opt = projected_reborrow_anchor_key
                                .as_ref()
                                .and_then(|key| projected_reborrow_anchor_local_for_key.get(key))
                                .copied()
                                .or_else(|| {
                                    if src.projection.is_empty() {
                                        reborrow_anchor_local_for_stack_local
                                            .get(&src.local)
                                            .copied()
                                    } else {
                                        None
                                    }
                                });
                            let anchor_state_local_opt = if src.projection.is_empty() {
                                anchor_is_slot_family_local_for_stack_local
                                    .get(&src.local)
                                    .copied()
                            } else {
                                None
                            };
                            if let Some(anchor_local) = anchor_local_opt {
                                if let Some(anchor_source_local) = projected_ref_parent_local
                                    .or_else(|| tag_local_for_ptr_local.get(&dst_local).copied())
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    let mut stmts = vec![
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
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ];
                                    if let Some(anchor_state_local) = anchor_state_local_opt {
                                        stmts.push(Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_state_local),
                                                Rvalue::Use(self.const_u8(
                                                    tcx,
                                                    source_info.span,
                                                    1,
                                                )),
                                            ))),
                                        ));
                                    }
                                    stmts
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if src.projection.is_empty() {
                            if let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                                .get(&src.local)
                                .copied()
                            {
                                if let Some(anchor_source_local) =
                                    tag_local_for_ptr_local.get(&dst_local).copied()
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    vec![
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
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ]
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };

            let split_at = {
                let len = body.basic_blocks[bb].statements.len();
                if stmt_idx >= len {
                    len
                } else if insert_before {
                    stmt_idx
                } else {
                    stmt_idx + 1
                }
            };
            let split_at_prefix = split_at.min(orig_stmt_len);
            orig_stmt_prefix_len.insert(bb, split_at_prefix);
            orig_stmt_prefix_len.insert(cont_block, orig_stmt_len.saturating_sub(split_at_prefix));

            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

                let rem = bd.statements.split_off(split_at);

                if let Some(s1) = addr_stmt1_opt {
                    bd.statements.push(s1);
                }
                bd.statements.push(addr_stmt2);
                if let Some(s3) = addr_adjust_stmt_opt {
                    bd.statements.push(s3);
                }
                if !extra_stmts.is_empty() {
                    bd.statements.extend(extra_stmts);
                }

                bd.terminator = Some(call_term);
                rem
            };

            if !reborrow_anchor_set_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, reborrow_anchor_set_stmts);
            }
            if !export_parent_init_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, export_parent_init_stmts);
            }
            if let Some(ref_ancestor_init_stmt) = ref_ancestor_init_stmt_opt {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .insert(0, ref_ancestor_init_stmt);
            }
            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
        }

        // Insert ArgRetag points after all other instrumentation.
        // Because each insertion rewrites the entry terminator, the last-applied
        // retag runs first at runtime, ensuring tags are initialized before reads.
        for (_idx, ip) in arg_retag_points.into_iter().rev() {
            let bb = ip.bb;
            let stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            if let InstrKind::FnEnter { callee_id } = creation_kind {
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let enter_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_enter_fn,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![Spanned {
                            node: self.const_u64(tcx, source_info.span, callee_id),
                            span: source_info.span,
                        }]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = stmt_idx.min(bd.statements.len());
                    let rem = bd.statements.split_off(split_at);
                    bd.terminator = Some(enter_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorSeedFromShadow { local } = creation_kind {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorSeedFromShadow");
                let anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&local)
                    .copied();
                let seed_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    seed_addr_local,
                    false,
                ) else {
                    continue;
                };
                let seed_ptr_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((seed_ptr_addr_stmt1_opt, seed_ptr_addr_stmt2)) =
                    self.addr_stmts_for_place(tcx, body, source_info, place, seed_ptr_addr_local)
                else {
                    continue;
                };
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let load_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_shadow_load_tag_for_ptr,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: Operand::Copy(Place::from(seed_addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(seed_ptr_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(anchor_local),
                        target: Some(cont_block),
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
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    if let Some(seed_ptr_addr_stmt1) = seed_ptr_addr_stmt1_opt {
                        bd.statements.push(seed_ptr_addr_stmt1);
                    }
                    bd.statements.push(seed_ptr_addr_stmt2);
                    bd.terminator = Some(load_term);
                    rem
                };
                if let Some(anchor_state_local) = anchor_state_local {
                    let anchor_nonzero_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                    let anchor_state_u8_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u8, source_info.span));
                    body.basic_blocks_mut()[cont_block].statements.extend([
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_nonzero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Ne,
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
                                Place::from(anchor_state_u8_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(anchor_nonzero_local)),
                                    tcx.types.u8,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_state_u8_local))),
                            ))),
                        ),
                    ]);
                }
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
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
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let ptr_ty = body.local_decls[ptr_local].ty;
                let bounds_len_op = if matches!(ptr_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        ptr_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
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
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, recovered_value)),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let local_ty = body.local_decls[local].ty;
                if self.is_box_ty(tcx, local_ty) {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (take_addr_stmt1, take_addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let pointee_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let Some((pointee_addr_stmt1, pointee_addr_stmt2)) = self
                        .box_pointee_slot_addr_stmts_for_local(
                            tcx,
                            body,
                            source_info,
                            local,
                            pointee_addr_local,
                        )
                    else {
                        panic!("ArgAnchorTake box local missing pointee address support");
                    };

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
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(pointee_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 1),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
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
                        bd.statements.push(take_addr_stmt1);
                        bd.statements.push(take_addr_stmt2);
                        bd.statements.push(pointee_addr_stmt1);
                        bd.statements.push(pointee_addr_stmt2);
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                } else {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (addr_stmt1, addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let Some((field_place, pointee_ty, is_mut)) =
                        self.first_direct_ref_field_place(tcx, body, local)
                    else {
                        // By-value call-boundary anchors are only valid for aggregates that
                        // structurally carry a source-level reference. If we cannot recover such a
                        // field, skip the synthetic root ref instead of inventing one for the
                        // whole slot.
                        continue;
                    };
                    let record_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (record_addr_stmt1_opt, record_addr_stmt2) = self
                        .addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            field_place,
                            record_addr_local,
                        )
                        .expect("ArgAnchorTake direct ref field address");
                    let record_is_mut = is_mut;
                    let record_ty = pointee_ty;
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let size_op = self.size_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    let align_op =
                        self.align_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (align_len, mut align_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);

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
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(record_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(
                                        tcx,
                                        source_info.span,
                                        if record_is_mut { 1 } else { 0 },
                                    ),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: bounds_len,
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: align_len,
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
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
                        bd.statements.push(addr_stmt1);
                        bd.statements.push(addr_stmt2);
                        if let Some(record_addr_stmt1) = record_addr_stmt1_opt {
                            bd.statements.push(record_addr_stmt1);
                        }
                        bd.statements.push(record_addr_stmt2);
                        if !bounds_len_stmts.is_empty() {
                            bd.statements.append(&mut bounds_len_stmts);
                        }
                        if !align_len_stmts.is_empty() {
                            bd.statements.append(&mut align_len_stmts);
                        }
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                }
            }

            if let InstrKind::ArgLeafTake {
                callee_id,
                arg_index,
                leaf_key,
            } = creation_kind
            {
                let take_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        take_addr_local,
                        false,
                    )
                    .expect("ArgLeafTake on unsupported local leaf slot");
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

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
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_call_arg_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(take_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
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
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    bd.terminator = Some(take_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
            }
        }
    }

    pub(in crate::instrumentation) fn insert_fallback_return_stack_allocs<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        fallback_return_locals: &[(Local, SizeOperand<'tcx>)],
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        if fallback_return_locals.is_empty() {
            return;
        }

        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            if !matches!(term.kind, TerminatorKind::Return) {
                continue;
            }

            for (local, size_op) in fallback_return_locals.iter().cloned() {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(local),
                    kind: InstrKind::StackAlloc {
                        local,
                        live: false,
                        size_op: size_op.clone(),
                    },
                });
                self.trace_stack_alloc_emit(tcx, body, local, false, &size_op, "FallbackReturn");
            }
        }

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            manual_holder_managed_tag_assignments,
            tag_local_for_ptr_local,
            local_slot_shadow_store_locals,
            export_parent_local_for_ptr_local,
            export_parent_is_recovered_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
            reborrow_anchor_local_for_stack_local,
            anchor_is_slot_family_local_for_stack_local,
            projectionless_anchor_suppressed_locals,
            projected_reborrow_anchor_local_for_key,
            debug_ref_bindings,
            hooks,
        );
    }
}
