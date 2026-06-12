//! Computes type sizes and lowers size operands for runtime calls.

use super::super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn ty_has_runtime_stack_slot_extent<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..) | TyKind::Foreign(..) => false,
            _ if self.ty_is_opaque_for_shadow_range(ty) => true,
            _ => ty.is_sized(tcx, body.typing_env(tcx)),
        }
    }

    pub(in crate::instrumentation) fn layout_size_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> usize {
        // `tcx.layout_of(...)` can trigger normalization and will hard-error (E0080)
        // for types that are not fully normalizable in the current context, e.g.
        // `&[<I as Iterator>::Item; 0]` inside generic code like `Splice<'_, I, N>::drop`.
        //
        // For our instrumentation, "unknown size" is fine: we already treat size=0 as
        // best-effort and avoid precise OOB checks in that case.
        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return 0;
        }

        let input = PseudoCanonicalInput {
            typing_env: TypingEnv::fully_monomorphized(),
            value: ty,
        };
        tcx.layout_of(input)
            .ok()
            .map(|l| l.size.bytes() as usize)
            .unwrap_or(0)
    }

    pub(in crate::instrumentation) fn is_one_byte_sized_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        self.layout_size_bytes(tcx, ty) == 1
    }

    pub(in crate::instrumentation) fn size_operand_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if !ty.is_sized(tcx, body.typing_env(tcx)) {
            // TODO(wide-ptr): for unsized pointees (slice/str), use metadata length to compute
            // access size instead of returning 0. This would enable precise OOB checks for
            // `*const [T]` / `*const str` derefs and indexing.
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }
        // Emit MIR size_of to avoid layout normalization during instrumentation.
        SizeOperand::SizeOf(ty)
    }

    pub(in crate::instrumentation) fn align_operand_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let typing_env = body.typing_env(tcx);
        if ty.is_sized(tcx, typing_env) {
            return SizeOperand::AlignOf(ty);
        }
        match ty.kind() {
            TyKind::Slice(elem_ty) => self.align_operand_for_ty(tcx, body, *elem_ty, span),
            TyKind::Str => SizeOperand::Const(self.const_usize(tcx, span, 1)),
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    /// Compute stack-slot size for a local type.
    ///
    /// For local stack slots we want to preserve `SizeOf(ty)` even for generic ADTs where
    /// `ty.is_sized(...)` can be inconclusive during instrumentation. Those locals are still
    /// sized once monomorphized, and dropping them to size 0 loses the surrounding stack
    /// allocation metadata needed for interior references.
    pub(in crate::instrumentation) fn size_operand_for_stack_local_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if self.ty_has_runtime_stack_slot_extent(tcx, body, ty) {
            SizeOperand::SizeOf(ty)
        } else {
            SizeOperand::Const(self.const_usize(tcx, span, 0))
        }
    }

    /// Stack-slot tracking should cover ordinary fixed-size locals as well as dynamic ones.
    /// The only stack sizes we intentionally suppress are the synthetic `size=0` operands used
    /// by the pass as "unknown/unsupported stack extent" sentinels.
    pub(in crate::instrumentation) fn should_emit_stack_alloc_for_size_op<'tcx>(
        &self,
        size_op: &SizeOperand<'tcx>,
    ) -> bool {
        match size_op {
            SizeOperand::Const(Operand::Constant(c)) => !matches!(
                c.const_,
                Const::Val(ConstValue::Scalar(Scalar::Int(int)), _)
                    if int.to_bits(int.size()) == 0
            ),
            SizeOperand::Const(_) => true,
            _ => true,
        }
    }

    /// Compute access size for a deref of `ptr_local` producing `access_ty`.
    /// For wide pointers to slices/str, derive the size from pointer metadata.
    pub(in crate::instrumentation) fn size_operand_for_deref<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        access_ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if access_ty.is_sized(tcx, body.typing_env(tcx)) {
            return self.size_operand_for_ty(tcx, body, access_ty, span);
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return SizeOperand::Const(self.const_usize(tcx, span, 0)),
        };

        match pointee.kind() {
            TyKind::Slice(elem_ty) => SizeOperand::PtrMetadataSlice {
                ptr_local,
                elem_ty: *elem_ty,
            },
            TyKind::Str => SizeOperand::PtrMetadataStr { ptr_local },
            TyKind::Adt(..) => match self.adt_slice_tail(tcx, body, pointee) {
                Some((field_idx, elem_ty)) => SizeOperand::PtrMetadataAdtSlice {
                    ptr_local,
                    adt_ty: pointee,
                    field_idx,
                    elem_ty,
                },
                None => SizeOperand::Const(self.const_usize(tcx, span, 0)),
            },
            _ => SizeOperand::Const(self.const_usize(tcx, span, 0)),
        }
    }

    /// Alignment for the actual projected access, not the root pointer type.
    ///
    /// Example: `(*p).byte` may need align 1 even when `*p` has align 8.
    pub(in crate::instrumentation) fn align_operand_for_deref_access<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        access_place: Place<'tcx>,
        access_ty: Ty<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if self.place_may_cross_packed_field(tcx, body, access_place) {
            return SizeOperand::Const(self.const_usize(tcx, span, 1));
        }
        self.align_operand_for_ty(tcx, body, access_ty, span)
    }

    pub(in crate::instrumentation) fn align_operand_for_ptr_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return SizeOperand::Const(self.const_usize(tcx, span, 0)),
        };
        self.align_operand_for_ty(tcx, body, pointee, span)
    }

    pub(in crate::instrumentation) fn align_operand_for_ptr_derive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Local,
        dst: Local,
        is_ref: bool,
        span: Span,
    ) -> SizeOperand<'tcx> {
        if is_ref {
            let dst_ty = body.local_decls[dst].ty;
            if matches!(
                dst_ty.kind(),
                TyKind::Ref(_, pointee, _) if matches!(pointee.kind(), TyKind::Dynamic(..))
            ) {
                return self.align_operand_for_ptr_local(tcx, body, src, span);
            }
        }
        self.align_operand_for_ptr_local(tcx, body, dst, span)
    }

    pub(in crate::instrumentation) fn align_operand_for_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Place<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let mut place_ty = PlaceTy::from_ty(body.local_decls[src.local].ty);
        for proj in src.projection.iter() {
            if let ProjectionElem::Field(..) = proj {
                if let TyKind::Adt(adt_def, _) = place_ty.ty.kind() {
                    if adt_def.repr().packed() {
                        return SizeOperand::Const(self.const_usize(tcx, span, 1));
                    }
                }
            }
            if matches!(proj, ProjectionElem::Deref) {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
            place_ty = place_ty.projection_ty(tcx, proj.clone());
        }
        self.align_operand_for_ty(tcx, body, place_ty.ty, span)
    }

    pub(in crate::instrumentation) fn align_operand_for_ref_creation_src_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        dst_ty: Ty<'tcx>,
        src: Place<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let src_ty = src.ty(&body.local_decls, tcx).ty;
        match src_ty.kind() {
            // When we copy/load an existing pointer value into a new `&T`, the creation hook
            // must validate the loaded reference against the pointee alignment, not the source
            // slot alignment of the reference object itself.
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if self.place_may_cross_packed_field(tcx, body, src) {
                    SizeOperand::Const(self.const_usize(tcx, span, 1))
                } else if matches!(
                    dst_ty.kind(),
                    TyKind::Ref(_, dst_pointee, _) if *dst_pointee == src_ty
                ) {
                    self.align_operand_for_src_place(tcx, body, src, span)
                } else {
                    self.align_operand_for_ty(tcx, body, *pointee, span)
                }
            }
            _ => {
                if self.place_may_cross_packed_field(tcx, body, src) {
                    SizeOperand::Const(self.const_usize(tcx, span, 1))
                } else if self.place_contains_deref(src) {
                    match dst_ty.kind() {
                        TyKind::Ref(_, dst_pointee, _) => {
                            self.align_operand_for_ty(tcx, body, *dst_pointee, span)
                        }
                        _ => self.align_operand_for_src_place(tcx, body, src, span),
                    }
                } else {
                    self.align_operand_for_src_place(tcx, body, src, span)
                }
            }
        }
    }
}
