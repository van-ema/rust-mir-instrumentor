//! Computes bounds information for sized and wide pointer values.

use super::super::*;

impl MyOptimizationPass {
    /// For a struct DST with a trailing `[T]` field, return the field index and element type.
    /// Other DST forms (`dyn Trait`, nested DST) return None.
    pub(in crate::instrumentation) fn adt_slice_tail<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        adt_ty: Ty<'tcx>,
    ) -> Option<(FieldIdx, Ty<'tcx>)> {
        let (adt_def, substs) = match adt_ty.kind() {
            TyKind::Adt(adt_def, substs) if adt_def.is_struct() => (adt_def, substs),
            _ => return None,
        };
        let variant = adt_def.non_enum_variant();
        let last_idx = variant.fields.len().checked_sub(1)?;
        let field = &variant.fields[FieldIdx::from_usize(last_idx)];
        let field_ty = field.ty(tcx, substs);
        let typing_env = body.typing_env(tcx);
        if field_ty.is_sized(tcx, typing_env) {
            return None;
        }
        match field_ty.kind() {
            TyKind::Slice(elem_ty) => Some((FieldIdx::from_usize(last_idx), *elem_ty)),
            _ => None,
        }
    }

    /// Best-effort bounds length for wide pointers (slice/str), in bytes.
    /// Returns `usize::MAX` for thin pointers or unknown metadata.
    pub(in crate::instrumentation) fn bounds_len_operand_for_ptr_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return self.unknown_bounds_len_operand(tcx, span),
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
                None => self.unknown_bounds_len_operand(tcx, span),
            },
            _ => self.unknown_bounds_len_operand(tcx, span),
        }
    }

    /// Bounds length for direct reference creation.
    ///
    /// Unlike raw derives, an `Rvalue::Ref` still names the concrete pointee object, so for sized
    /// pointees we can safely use `size_of::<T>()` as the dynamic bounds. This preserves array /
    /// aggregate extents across later unsizing or vectorized loads (e.g. `&[u8; 64]` feeding SIMD
    /// lane reads) without constraining subsequent raw-pointer arithmetic.
    pub(in crate::instrumentation) fn ref_creation_bounds_len_operand_for_ptr_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let pointee = match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) => *pointee,
            _ => return self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, span),
        };

        match pointee.kind() {
            TyKind::Slice(elem_ty) => SizeOperand::PtrMetadataSlice {
                ptr_local,
                elem_ty: *elem_ty,
            },
            TyKind::Str => SizeOperand::PtrMetadataStr { ptr_local },
            TyKind::Adt(..) if !pointee.is_sized(tcx, body.typing_env(tcx)) => {
                match self.adt_slice_tail(tcx, body, pointee) {
                    Some((field_idx, elem_ty)) => SizeOperand::PtrMetadataAdtSlice {
                        ptr_local,
                        adt_ty: pointee,
                        field_idx,
                        elem_ty,
                    },
                    None => self.unknown_bounds_len_operand(tcx, span),
                }
            }
            _ if pointee.is_sized(tcx, body.typing_env(tcx)) => {
                SizeOperand::RefSizedBoundsLen(pointee)
            }
            _ => self.unknown_bounds_len_operand(tcx, span),
        }
    }

    #[inline]
    pub(in crate::instrumentation) fn unknown_bounds_len_operand<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        SizeOperand::Const(self.const_usize(tcx, span, usize::MAX))
    }

    pub(in crate::instrumentation) fn ptr_ty_has_precise_wide_bounds<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str) {
                    return true;
                }
                if matches!(pointee.kind(), TyKind::Adt(..)) {
                    return self.adt_slice_tail(tcx, body, *pointee).is_some();
                }
                false
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn should_forward_bounds_from_src_ptr_derive<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src: Local,
        dst: Local,
    ) -> bool {
        let src_ty = body.local_decls[src].ty;
        let dst_ty = body.local_decls[dst].ty;
        self.ptr_ty_has_precise_wide_bounds(tcx, body, src_ty)
            && self.is_thin_ptr_ty(tcx, body, dst_ty)
    }
}
