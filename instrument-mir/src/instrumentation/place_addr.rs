//! Builds MIR places and address operands used by runtime hooks.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn place_from_operand<'tcx>(
        &self,
        op: &Operand<'tcx>,
    ) -> Option<Place<'tcx>> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => Some(*p),
            _ => None,
        }
    }
    pub(in crate::instrumentation) fn pointer_place_from_rvalue<'tcx>(
        &self,
        rvalue: &Rvalue<'tcx>,
    ) -> Option<Place<'tcx>> {
        match rvalue {
            Rvalue::Use(Operand::Copy(place)) | Rvalue::Use(Operand::Move(place)) => Some(*place),
            Rvalue::CopyForDeref(place) => Some(*place),
            _ => None,
        }
    }
    pub(in crate::instrumentation) fn place_contains_deref<'tcx>(
        &self,
        place: Place<'tcx>,
    ) -> bool {
        place
            .projection
            .iter()
            .any(|proj| matches!(proj, ProjectionElem::Deref))
    }

    pub(in crate::instrumentation) fn pointer_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_local: Local,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        Place::from(base_local).project_deeper(
            &[PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty)],
            tcx,
        )
    }

    pub(in crate::instrumentation) fn pointer_field_place_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_place: Place<'tcx>,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        base_place.project_deeper(
            &[PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty)],
            tcx,
        )
    }

    pub(in crate::instrumentation) fn pointer_field_place_from_place_in_variant<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        base_place: Place<'tcx>,
        variant: Option<VariantIdx>,
        field_idx: usize,
        field_ty: Ty<'tcx>,
    ) -> Place<'tcx> {
        let mut elems = Vec::with_capacity(1 + usize::from(variant.is_some()));
        if let Some(variant_idx) = variant {
            elems.push(PlaceElem::Downcast(None, variant_idx));
        }
        elems.push(PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty));
        base_place.project_deeper(&elems, tcx)
    }

    pub(in crate::instrumentation) fn aggregate_field_tys<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        dst_ty: Ty<'tcx>,
    ) -> Option<Vec<Ty<'tcx>>> {
        match dst_ty.kind() {
            TyKind::Tuple(field_tys) => Some(field_tys.iter().collect()),
            TyKind::Adt(adt, args) if adt.is_struct() => Some(
                adt.non_enum_variant()
                    .fields
                    .iter()
                    .map(|field| field.ty(tcx, args))
                    .collect(),
            ),
            _ => None,
        }
    }
    pub(in crate::instrumentation) fn first_direct_ref_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> Option<(Place<'tcx>, Ty<'tcx>, bool)> {
        let local_ty = body.local_decls[local].ty;
        let field_tys = self.aggregate_field_tys(tcx, local_ty)?;
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            let TyKind::Ref(_, pointee_ty, mutbl) = field_ty.kind() else {
                continue;
            };
            let field_place = self.pointer_field_place(tcx, local, field_idx, field_ty);
            return Some((field_place, *pointee_ty, matches!(mutbl, Mutability::Mut)));
        }
        None
    }

    pub(in crate::instrumentation) fn first_direct_pointer_field_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> Option<Place<'tcx>> {
        let local_ty = body.local_decls[local].ty;
        let field_tys = self.aggregate_field_tys(tcx, local_ty)?;
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            if self.is_shadowable_ptr_ty(tcx, body, field_ty) {
                return Some(self.pointer_field_place(tcx, local, field_idx, field_ty));
            }
        }
        None
    }

    pub(in crate::instrumentation) fn single_direct_pointer_leaf_place_for_projectionless_carrier<
        'tcx,
    >(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> Option<Place<'tcx>> {
        if !src_place.projection.is_empty() {
            return None;
        }

        let local = src_place.local;
        let local_ty = body.local_decls[local].ty;
        if self.is_pointer_ty(local_ty)
            || !self.ty_contains_direct_pointer_fields(tcx, body, local_ty)
        {
            return None;
        }

        let leaf_specs = self.shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, local_ty);
        if leaf_specs.len() != 1 {
            return None;
        }

        let field_place = self.first_direct_pointer_field_place(tcx, body, local)?;
        (leaf_specs[0].place == field_place).then_some(field_place)
    }

    pub(in crate::instrumentation) fn single_shadowable_leaf_place_for_pointer_pointee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_place: Place<'tcx>,
    ) -> Option<Place<'tcx>> {
        let (pointee_place, pointee_ty) =
            self.pointer_pointee_place_and_ty(tcx, body, ptr_place)?;
        let leaf_specs =
            self.shadowable_leaf_ptr_specs_from_place(tcx, body, pointee_place, pointee_ty);
        (leaf_specs.len() == 1).then_some(leaf_specs[0].place)
    }

    pub(in crate::instrumentation) fn leaf_path_key_child(
        parent_key: u64,
        field_idx: usize,
    ) -> u64 {
        parent_key
            .wrapping_mul(131)
            .wrapping_add(field_idx as u64 + 1)
    }

    pub(in crate::instrumentation) fn collect_shadowable_leaf_ptr_specs_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        depth: usize,
        byte_offset: Option<u64>,
        path_key: u64,
        out: &mut Vec<ShadowableLeafPtrSpec<'tcx>>,
    ) {
        let place_ty = base_place.ty(&body.local_decls, tcx);
        let ty = place_ty.ty;
        if self.is_shadowable_ptr_ty(tcx, body, ty) {
            out.push(ShadowableLeafPtrSpec {
                place: base_place,
                ty,
                byte_offset,
                path_key,
            });
            return;
        }
        if depth == 0 {
            return;
        }
        let Some(field_tys) = self.aggregate_field_tys(tcx, ty) else {
            return;
        };
        for (field_idx, field_ty) in field_tys.into_iter().enumerate() {
            if !self.is_shadowable_ptr_ty(tcx, body, field_ty)
                && !self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)
            {
                continue;
            }
            let field_place =
                self.pointer_field_place_from_place(tcx, base_place, field_idx, field_ty);
            let field_offset = self.field_offset_bytes(
                tcx,
                body,
                place_ty.ty,
                place_ty.variant_index,
                FieldIdx::from_usize(field_idx),
            );
            let child_offset = match (byte_offset, field_offset) {
                (Some(base), Some(field)) => Some(base.wrapping_add(field)),
                _ => None,
            };
            self.collect_shadowable_leaf_ptr_specs_from_place(
                tcx,
                body,
                field_place,
                depth - 1,
                child_offset,
                Self::leaf_path_key_child(path_key, field_idx),
                out,
            );
        }
    }

    pub(in crate::instrumentation) fn shadowable_leaf_ptr_specs_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        _ty: Ty<'tcx>,
    ) -> Vec<ShadowableLeafPtrSpec<'tcx>> {
        let mut out = Vec::new();
        self.collect_shadowable_leaf_ptr_specs_from_place(
            tcx,
            body,
            base_place,
            SHADOWABLE_LEAF_PTR_RECURSION_DEPTH,
            Some(0),
            0,
            &mut out,
        );
        out
    }

    pub(in crate::instrumentation) fn pointer_pointee_place_and_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_place: Place<'tcx>,
    ) -> Option<(Place<'tcx>, Ty<'tcx>)> {
        let ptr_ty = ptr_place.ty(&body.local_decls, tcx).ty;
        let pointee_ty = match ptr_ty.kind() {
            TyKind::Ref(_, pointee_ty, _) => *pointee_ty,
            TyKind::RawPtr(pointee_ty, _) => *pointee_ty,
            _ => return None,
        };
        Some((
            ptr_place.project_deeper(&[PlaceElem::Deref], tcx),
            pointee_ty,
        ))
    }

    pub(in crate::instrumentation) fn shadowable_leaf_ptr_places_from_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_place: Place<'tcx>,
        ty: Ty<'tcx>,
    ) -> Vec<(Place<'tcx>, Ty<'tcx>)> {
        self.shadowable_leaf_ptr_specs_from_place(tcx, body, base_place, ty)
            .into_iter()
            .map(|spec| (spec.place, spec.ty))
            .collect()
    }

    pub(in crate::instrumentation) fn raw_creation_allows_no_provenance_transport<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
    ) -> bool {
        if !matches!(src.projection.first(), Some(ProjectionElem::Deref)) {
            return false;
        }

        let mut cur_place = Place::from(src.local);
        for proj in src.projection.iter() {
            let cur_ty = cur_place.ty(&body.local_decls, tcx).ty;
            if let ProjectionElem::Field(_, field_ty) = proj {
                if !self.is_pointer_ty(cur_ty)
                    && (self.is_pointer_ty(field_ty)
                        || self.ty_contains_pointer_fields(tcx, body, field_ty, 2))
                {
                    return true;
                }
            }
            cur_place = cur_place.project_deeper(&[proj], tcx);
        }

        false
    }
    /// Build statements that compute `addr_local` from a pointer-typed place.
    ///
    /// - For thin pointers we can `PointerExposeProvenance` directly.
    /// - For wide pointers (`&[T]`, `&str`, `dyn Trait`), we first cast to a thin raw pointer
    ///   to unit (`*const ()` / `*mut ()`) to drop metadata, then expose provenance from the
    ///   thin data pointer.
    pub(in crate::instrumentation) fn addr_stmts_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
    ) -> Option<(Option<Statement<'tcx>>, Statement<'tcx>)> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_thin_ptr_ty(tcx, body, place_ty)
            || self.ptr_ty_has_sized_pointee(tcx, body, place_ty)
        {
            let addr_stmt = Statement::new(
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
            return Some((None, addr_stmt));
        }

        // Wide pointer: extract data pointer first.
        let data_ptr_ty = self.data_ptr_ty_for_ptr(tcx, place_ty)?;
        let data_ptr_local = body
            .local_decls
            .push(LocalDecl::new(data_ptr_ty, source_info.span));

        let data_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(data_ptr_local),
                Rvalue::Cast(CastKind::PtrToPtr, Operand::Copy(place), data_ptr_ty),
            ))),
        );

        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(data_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((Some(data_ptr_stmt), addr_stmt))
    }

    pub(in crate::instrumentation) fn slot_addr_stmts_for_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
        is_mut: bool,
    ) -> Option<(Statement<'tcx>, Statement<'tcx>)> {
        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if !place_ty.is_sized(tcx, body.typing_env(tcx)) {
            return None;
        }

        let raw_ptr_ty = if is_mut {
            Ty::new_mut_ptr(tcx, place_ty)
        } else {
            Ty::new_imm_ptr(tcx, place_ty)
        };
        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
            return None;
        }

        let slot_ptr_local = body
            .local_decls
            .push(LocalDecl::new(raw_ptr_ty, source_info.span));

        let slot_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(slot_ptr_local),
                Rvalue::RawPtr(
                    if is_mut {
                        RawPtrKind::Mut
                    } else {
                        RawPtrKind::Const
                    },
                    place,
                ),
            ))),
        );

        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(slot_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((slot_ptr_stmt, addr_stmt))
    }
    /// Compute MIR statements that recover the heap payload address stored inside a `Box<T>` local.
    ///
    /// This is specifically for `ShadowStoreBoxPointee`: after a call such as `Box::new(p)`, we
    /// need the address of the box pointee storage so we can write `p`'s shadow tag metadata into
    /// that heap slot. The helper walks the `Box<T>` representation down to its internal
    /// `NonNull<T>`, converts it to a raw byte pointer, and then exposes provenance to obtain the
    /// slot address as `usize`.
    pub(in crate::instrumentation) fn box_pointee_slot_addr_stmts_for_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        box_local: Local,
        addr_local: Local,
    ) -> Option<(Statement<'tcx>, Statement<'tcx>)> {
        let box_ty = body.local_decls[box_local].ty;
        let TyKind::Adt(box_adt, box_args) = box_ty.kind() else {
            return None;
        };
        if !self.is_box_ty(tcx, box_ty) {
            return None;
        }

        let unique_ty =
            box_adt.non_enum_variant().fields[FieldIdx::from_usize(0)].ty(tcx, box_args);
        let TyKind::Adt(unique_adt, unique_args) = unique_ty.kind() else {
            return None;
        };
        let nonnull_ty =
            unique_adt.non_enum_variant().fields[FieldIdx::from_usize(0)].ty(tcx, unique_args);

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, tcx.types.u8);
        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
            return None;
        }

        let nonnull_place = Place::from(box_local).project_deeper(
            &[
                PlaceElem::Field(FieldIdx::from_usize(0), unique_ty),
                PlaceElem::Field(FieldIdx::from_usize(0), nonnull_ty),
            ],
            tcx,
        );
        let slot_ptr_local = body
            .local_decls
            .push(LocalDecl::new(raw_ptr_ty, source_info.span));

        let slot_ptr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(slot_ptr_local),
                Rvalue::Cast(
                    CastKind::Transmute,
                    Operand::Copy(nonnull_place),
                    raw_ptr_ty,
                ),
            ))),
        );
        let addr_stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(addr_local),
                Rvalue::Cast(
                    CastKind::PointerExposeProvenance,
                    Operand::Copy(Place::from(slot_ptr_local)),
                    tcx.types.usize,
                ),
            ))),
        );

        Some((slot_ptr_stmt, addr_stmt))
    }
    pub(in crate::instrumentation) fn place_may_cross_packed_field<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
    ) -> bool {
        let mut place_ty = PlaceTy::from_ty(body.local_decls[src.local].ty);
        for proj in src.projection.iter() {
            if let ProjectionElem::Field(..) = proj {
                if let TyKind::Adt(adt_def, _) = place_ty.ty.kind() {
                    if adt_def.repr().packed() {
                        return true;
                    }
                }
            }
            place_ty = place_ty.projection_ty(tcx, proj.clone());
        }
        false
    }
    pub(in crate::instrumentation) fn field_offset_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        base_ty: Ty<'tcx>,
        variant: Option<VariantIdx>,
        field: FieldIdx,
    ) -> Option<u64> {
        if base_ty.has_param()
            || base_ty.has_infer()
            || base_ty.has_aliases()
            || base_ty.has_opaque_types()
            || base_ty.has_placeholders()
        {
            return None;
        }

        let typing_env = body.typing_env(tcx);
        let input = PseudoCanonicalInput {
            typing_env,
            value: base_ty,
        };
        let layout = tcx.layout_of(input).ok()?;
        let cx = rustc_middle::ty::layout::LayoutCx::new(tcx, typing_env);
        let layout = if let Some(variant_idx) = variant {
            layout.for_variant(&cx, variant_idx)
        } else {
            layout
        };

        Some(layout.fields.offset(field.index()).bytes())
    }
    pub(in crate::instrumentation) fn cast_index_to_usize<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        idx_op: Operand<'tcx>,
        idx_ty: Ty<'tcx>,
    ) -> (Operand<'tcx>, Vec<Statement<'tcx>>) {
        if idx_ty == tcx.types.usize {
            return (idx_op, Vec::new());
        }
        let idx_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.usize, source_info.span));
        let stmt = Statement::new(
            source_info,
            StatementKind::Assign(Box::new((
                Place::from(idx_local),
                Rvalue::Cast(CastKind::IntToInt, idx_op, tcx.types.usize),
            ))),
        );
        (Operand::Copy(Place::from(idx_local)), vec![stmt])
    }

    pub(in crate::instrumentation) fn offset_stmts_for_projection<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        base_ptr_local: Local,
        projection: &[PlaceElem<'tcx>],
        addr_local: Local,
    ) -> Option<Vec<Statement<'tcx>>> {
        if projection.is_empty() {
            return Some(Vec::new());
        }

        let base_ptr_ty = body.local_decls[base_ptr_local].ty;
        let base_pointee = base_ptr_ty.builtin_deref(true)?;
        let mut place_ty = PlaceTy::from_ty(base_pointee);

        let mut offset_local: Option<Local> = None;
        let mut stmts: Vec<Statement<'tcx>> = Vec::new();

        let mut ensure_offset_local =
            |body: &mut Body<'tcx>, stmts: &mut Vec<Statement<'tcx>>| -> Local {
                if let Some(l) = offset_local {
                    return l;
                }
                let l = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let init_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(l),
                        Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                    ))),
                );
                stmts.push(init_stmt);
                offset_local = Some(l);
                l
            };

        for proj in projection.iter() {
            match proj {
                ProjectionElem::Deref => {
                    // Nested deref: we do not try to follow multiple levels here.
                    return None;
                }
                ProjectionElem::Downcast(_, _variant_idx) => {
                    // Keep `place_ty` unchanged here and let the canonical
                    // `projection_ty` update happen once at the end of the loop.
                    // Setting `variant_index` manually here and then calling
                    // `projection_ty(Downcast)` again triggers rustc's
                    // "non field projection on downcasted place" ICE.
                }
                ProjectionElem::Field(field, _) => {
                    let offset_bytes = self.field_offset_bytes(
                        tcx,
                        body,
                        place_ty.ty,
                        place_ty.variant_index,
                        *field,
                    )?;
                    if offset_bytes != 0 {
                        let off_local = ensure_offset_local(body, &mut stmts);
                        let add_stmt = Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(off_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(off_local)),
                                        self.const_usize(
                                            tcx,
                                            source_info.span,
                                            offset_bytes as usize,
                                        ),
                                    )),
                                ),
                            ))),
                        );
                        stmts.push(add_stmt);
                    }
                }
                ProjectionElem::Index(idx_local) => {
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_ty = body.local_decls[*idx_local].ty;
                    // `ProjectionElem::Index` is expected to use an integer local, but in
                    // complex optimized MIR we occasionally see non-scalar locals here.
                    // Bail out to the base-address fallback rather than emitting invalid MIR.
                    if !idx_ty.is_integral() {
                        return None;
                    }
                    let idx_op = Operand::Copy(Place::from(*idx_local));
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, idx_ty);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                ProjectionElem::ConstantIndex {
                    offset, from_end, ..
                } => {
                    if *from_end {
                        return None;
                    }
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_op = self.const_usize(tcx, source_info.span, *offset as usize);
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, tcx.types.usize);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                ProjectionElem::Subslice { from, from_end, .. } => {
                    if *from_end {
                        return None;
                    }
                    let elem_ty = place_ty.ty.builtin_index()?;
                    let off_local = ensure_offset_local(body, &mut stmts);
                    let idx_op = self.const_usize(tcx, source_info.span, *from as usize);
                    let (idx_usize_op, mut idx_stmts) =
                        self.cast_index_to_usize(tcx, body, source_info, idx_op, tcx.types.usize);
                    stmts.append(&mut idx_stmts);

                    let size_op = self.size_operand_for_ty(tcx, body, elem_ty, source_info.span);
                    let (elem_size_op, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    stmts.append(&mut size_stmts);

                    let mul_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let mul_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(mul_local),
                            Rvalue::BinaryOp(BinOp::Mul, Box::new((idx_usize_op, elem_size_op))),
                        ))),
                    );
                    stmts.push(mul_stmt);
                    let add_stmt = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(off_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(off_local)),
                                    Operand::Copy(Place::from(mul_local)),
                                )),
                            ),
                        ))),
                    );
                    stmts.push(add_stmt);
                }
                _ => return None,
            }

            place_ty = place_ty.projection_ty(tcx, *proj);
        }

        if let Some(off_local) = offset_local {
            let add_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(addr_local),
                    Rvalue::BinaryOp(
                        BinOp::Add,
                        Box::new((
                            Operand::Copy(Place::from(addr_local)),
                            Operand::Copy(Place::from(off_local)),
                        )),
                    ),
                ))),
            );
            stmts.push(add_stmt);
        }

        Some(stmts)
    }

    pub(in crate::instrumentation) fn addr_stmts_for_access_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        place: Place<'tcx>,
        addr_local: Local,
    ) -> Option<(
        Option<Statement<'tcx>>,
        Statement<'tcx>,
        Vec<Statement<'tcx>>,
    )> {
        let has_deref = place
            .projection
            .iter()
            .next()
            .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
        if has_deref {
            let base_place = Place::from(place.local);
            let (opt, stmt) =
                self.addr_stmts_for_place(tcx, body, source_info, base_place, addr_local)?;
            let offset_stmts = self.offset_stmts_for_projection(
                tcx,
                body,
                source_info,
                place.local,
                &place.projection[1..],
                addr_local,
            )?;
            return Some((opt, stmt, offset_stmts));
        }

        let place_ty = place.ty(&body.local_decls, tcx).ty;
        if self.is_pointer_ty(place_ty) {
            let (opt, stmt) =
                self.addr_stmts_for_place(tcx, body, source_info, place, addr_local)?;
            return Some((opt, stmt, Vec::new()));
        }

        None
    }
}
