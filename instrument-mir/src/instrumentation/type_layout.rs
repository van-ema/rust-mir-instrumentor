use super::*;

impl MyOptimizationPass {
    /// Return true for any pointer or reference type, including wide pointers like slices and str.
    /// We treat these as tag-carrying so that when MIR later extracts a thin data pointer, the
    /// original tag can be propagated instead of silently dropping to tag zero.
    pub(in crate::instrumentation) fn is_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
    }

    pub(in crate::instrumentation) fn is_raw_pointer_ty<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), TyKind::RawPtr(..))
    }

    pub(in crate::instrumentation) fn is_shadowable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        self.is_pointer_ty(ty)
            && (self.is_thin_ptr_ty(tcx, body, ty)
                || self.ptr_ty_has_precise_wide_bounds(tcx, body, ty))
    }

    /// Best-effort recursive check for whether `ty` contains any reference/raw-pointer field.
    ///
    /// This is used for non-pointer carrier values such as `Option<&T>`, tuples, or small wrapper
    /// structs so we can still add boundary validation/lineage handling even when the MIR local is
    /// not itself pointer-typed.
    pub(in crate::instrumentation) fn ty_contains_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        if self.is_pointer_ty(ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys
                .iter()
                .any(|field_ty| self.ty_contains_pointer_fields(tcx, body, field_ty, depth - 1)),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    self.ty_contains_pointer_fields(tcx, body, field.ty(tcx, args), depth - 1)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.ty_contains_pointer_fields(tcx, body, *elem_ty, depth - 1)
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ty_is_direct_pointer_wrapper<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        let path = tcx.def_path_str(adt.did());
        let is_wrapper = path.contains("::sync::atomic::AtomicPtr")
            || path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::mem::MaybeUninit")
            || path.contains("::mem::ManuallyDrop")
            || path.contains("::cell::Cell");
        if !is_wrapper {
            return false;
        }
        adt.non_enum_variant().fields.iter().any(|field| {
            let field_ty = field.ty(tcx, args);
            self.is_pointer_ty(field_ty)
                || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, depth - 1)
        })
    }

    /// Shallow structural check for non-pointer values that directly store pointer data.
    ///
    /// This is used for leaf shadow transport and similar structural handling. It intentionally
    /// does *not* imply that the outer value is a borrow carrier at call boundaries; raw-owner
    /// aggregates such as `Bytes`, `BytesMut`, `Vec`, or `Box` can satisfy this check while still
    /// being ineligible for ref-style boundary validation/export.
    pub(in crate::instrumentation) fn ty_contains_direct_pointer_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if self.is_pointer_ty(ty) {
            return true;
        }
        if self.ty_is_direct_pointer_wrapper(tcx, body, ty, 3) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                self.is_pointer_ty(field_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    let field_ty = field.ty(tcx, args);
                    self.is_pointer_ty(field_ty)
                        || self.ty_is_direct_pointer_wrapper(tcx, body, field_ty, 3)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                self.is_pointer_ty(*elem_ty)
                    || self.ty_is_direct_pointer_wrapper(tcx, body, *elem_ty, 3)
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ty_is_direct_ref_wrapper<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        let path = tcx.def_path_str(adt.did());
        let is_wrapper = path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::mem::MaybeUninit")
            || path.contains("::mem::ManuallyDrop")
            || path.contains("::cell::Cell");
        if !is_wrapper {
            return false;
        }
        adt.non_enum_variant().fields.iter().any(|field| {
            let field_ty = field.ty(tcx, args);
            matches!(field_ty.kind(), TyKind::Ref(..))
                || self.ty_is_direct_ref_wrapper(tcx, field_ty, depth - 1)
        })
    }

    /// Shallow boundary-carrier check for non-pointer values that directly carry a source-level
    /// reference. Raw-only owner/control aggregates are intentionally excluded.
    pub(in crate::instrumentation) fn ty_contains_direct_ref_fields<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if matches!(ty.kind(), TyKind::Ref(..)) {
            return true;
        }
        match ty.kind() {
            TyKind::Tuple(field_tys) => field_tys.iter().any(|field_ty| {
                matches!(field_ty.kind(), TyKind::Ref(..))
                    || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
            }),
            TyKind::Adt(adt, args) => adt.variants().iter().any(|variant| {
                variant.fields.iter().any(|field| {
                    let field_ty = field.ty(tcx, args);
                    matches!(field_ty.kind(), TyKind::Ref(..))
                        || self.ty_is_direct_ref_wrapper(tcx, field_ty, 3)
                })
            }),
            TyKind::Array(elem_ty, _) | TyKind::Slice(elem_ty) => {
                matches!(elem_ty.kind(), TyKind::Ref(..))
                    || self.ty_is_direct_ref_wrapper(tcx, *elem_ty, 3)
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn compile_alias_model_is_sb_like(&self) -> bool {
        std::env::var("RZ_ALIAS_MODEL")
            .ok()
            .map(|raw| raw.to_ascii_lowercase())
            .is_some_and(|model| matches!(model.as_str(), "sb" | "sb_lite" | "stacked_borrows"))
    }

    /// Best-effort detection of "vtable-like" structs: all fields are function pointers.
    pub(in crate::instrumentation) fn is_fn_table_adt_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        let TyKind::Adt(adt, args) = ty.kind() else {
            return false;
        };
        if !adt.is_struct() {
            return false;
        }
        let variant = adt.non_enum_variant();
        if variant.fields.is_empty() {
            return false;
        }

        for field in variant.fields.iter() {
            let fty = field.ty(tcx, args);
            match fty.kind() {
                TyKind::FnPtr(..) | TyKind::FnDef(..) => {}
                _ => return false,
            }
        }
        true
    }

    /// Best-effort detection of "vtable-like" pointers: pointers to structs whose
    /// fields are all function pointers. These typically live in static memory.
    pub(in crate::instrumentation) fn is_vtable_like_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        let pointee = match ty.kind() {
            TyKind::Ref(_, p, _) | TyKind::RawPtr(p, _) => *p,
            _ => return false,
        };
        self.is_fn_table_adt_ty(tcx, pointee)
    }

    /// Return true only for *thin* pointers (one machine word).
    ///
    /// IMPORTANT: do **not** call `tcx.layout_of` / `layout_size_bytes` here.
    /// During MIR instrumentation we may see generic/projection types that cannot be
    /// normalized yet (e.g. `&[<I as Iterator>::Item; 0]` inside `SmallVec`), and forcing a
    /// layout query can surface an `E0080` "unable to determine layout ... cannot be normalized"
    /// error during compilation.
    ///
    /// Instead, classify fat pointers syntactically by looking at the pointee type:
    /// references/raw-pointers to DSTs (`[T]`, `str`, `dyn Trait`) are fat; everything else is
    /// treated as thin.
    pub(in crate::instrumentation) fn is_thin_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                // Be conservative for unresolved/generic pointees: if we misclassify a fat pointer
                // as thin, `PointerExposeProvenance` on the pair-typed value can ICE during codegen.
                if pointee.has_param()
                    || pointee.has_infer()
                    || pointee.has_aliases()
                    || pointee.has_opaque_types()
                    || pointee.has_placeholders()
                    || pointee.has_bound_vars()
                {
                    return false;
                }

                match pointee.kind() {
                    TyKind::Slice(..) | TyKind::Str | TyKind::Dynamic(..) => false,
                    // `extern type` is unsized but uses `()` metadata, so pointers are thin.
                    TyKind::Foreign(..) => true,
                    _ => pointee.is_sized(tcx, body.typing_env(tcx)),
                }
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn ptr_ty_has_sized_pointee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                pointee.is_sized(tcx, body.typing_env(tcx))
            }
            _ => false,
        }
    }

    /// Return true when we can safely extract a concrete address from a pointer type.
    /// This is stricter than `is_thin_ptr_ty`: we also require the pointee to be sized
    /// in the current typing environment.
    pub(in crate::instrumentation) fn is_addr_exposable_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                (self.is_thin_ptr_ty(tcx, body, ty) || self.ptr_ty_has_sized_pointee(tcx, body, ty))
                    && pointee.is_sized(tcx, body.typing_env(tcx))
            }
            _ => false,
        }
    }

    /// Return true when call-boundary return tagging is safe and useful for this pointer type.
    ///
    /// We always include thin pointers. For wide pointers, we currently include slice/str
    /// pointers (metadata is a length and we can derive byte bounds precisely), but skip
    /// `dyn Trait`/other DST metadata forms to avoid conservative false positives.
    pub(in crate::instrumentation) fn supports_call_boundary_ret_tag_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                if self.is_addr_exposable_ptr_ty(tcx, body, ty) {
                    return true;
                }
                matches!(pointee.kind(), TyKind::Slice(..) | TyKind::Str)
            }
            _ => false,
        }
    }

    /// Produce a thin raw pointer type suitable for extracting the data pointer from a wide pointer.
    /// We only care about the address, so a pointer to unit keeps the correct size and mutability
    /// while discarding the metadata.
    pub(in crate::instrumentation) fn data_ptr_ty_for_ptr<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> Option<Ty<'tcx>> {
        match ptr_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) | TyKind::RawPtr(_ty, mutbl) => {
                let is_mut = matches!(mutbl, Mutability::Mut);
                Some(if is_mut {
                    Ty::new_mut_ptr(tcx, tcx.types.unit)
                } else {
                    Ty::new_imm_ptr(tcx, tcx.types.unit)
                })
            }
            _ => None,
        }
    }

    /// Extract mutability from a raw pointer or reference type.
    pub(in crate::instrumentation) fn ptr_is_mut<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        }
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
    /// Returns true when alias checks should be skipped for an accessed memory type.
    /// This is intentionally broad: accesses that touch an aggregate containing
    /// interior-mutability must stay exempt to avoid attributing writes to the
    /// wrong subfield.
    pub(in crate::instrumentation) fn alias_exempt_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        // SB-lite currently approximates Rust's aliasing rules using a per-allocation
        // borrow stack, but it does **not** model interior mutability soundly.
        //
        // In Rust, types that contain an `UnsafeCell` are *not* `Freeze`, meaning they
        // may be legally mutated through a shared reference (via `Cell`/`RefCell` or
        // other unsafe code patterns). Treating such accesses as ordinary shared reads
        // and enforcing "no write while shared is live" would yield many false positives
        // in real-world code (e.g., `bytes::BytesMut` internals).
        //
        // Policy: if a pointee type is not `Freeze` (or we cannot reliably reason about
        // it in this typing context), mark derived tags as `alias_exempt` so the runtime
        // skips SB-lite enforcement for that tag. This is a deliberate precision/soundness
        // trade-off: missing metadata is acceptable; incorrect metadata is not.
        // Keep alias checks enabled for slice/str pointees even in generic code:
        // `from_raw_parts_mut`-style wrappers are often generic and would otherwise
        // be fully exempt, hiding core aliasing violations.
        if matches!(ty.kind(), TyKind::Slice(_) | TyKind::Str) {
            return false;
        }

        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return true;
        }
        let typing_env = body.typing_env(tcx);
        !ty.is_freeze(tcx, typing_env)
    }

    /// Returns true when a pointee type is itself an interior-mutability root that should
    /// propagate alias exemption to a directly-derived child pointer.
    ///
    /// This is narrower than `alias_exempt_for_ty`: aggregates that merely *contain*
    /// an `UnsafeCell` (for example a struct with one `Cell` field) are not treated as
    /// alias-exempt roots for derived tags, otherwise a raw/shared tag to the whole
    /// aggregate would suppress alias checks for unrelated non-interior-mutable fields.
    pub(in crate::instrumentation) fn alias_exempt_root_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if matches!(ty.kind(), TyKind::Slice(_) | TyKind::Str) {
            return false;
        }

        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return true;
        }

        let typing_env = body.typing_env(tcx);
        if ty.is_freeze(tcx, typing_env) {
            return false;
        }

        match ty.kind() {
            TyKind::Adt(adt, _) => {
                let path = tcx.def_path_str(adt.did());
                path.contains("::cell::UnsafeCell")
                    || path.contains("::cell::SyncUnsafeCell")
                    || path.contains("::cell::Cell")
                    || path.contains("::cell::RefCell")
                    || path.contains("::pin::UnsafePinned")
            }
            TyKind::Tuple(_) | TyKind::Array(..) | TyKind::Slice(_) => false,
            _ => true,
        }
    }

    pub(in crate::instrumentation) fn alias_exempt_for_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, mutbl) => {
                if matches!(mutbl, Mutability::Mut) {
                    false
                } else {
                    self.alias_exempt_root_for_ty(tcx, body, *pointee)
                }
            }
            TyKind::RawPtr(pointee, _) => self.alias_exempt_root_for_ty(tcx, body, *pointee),
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn interior_mut_array_extent_for_source_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        src: Place<'tcx>,
    ) -> Option<(Operand<'tcx>, SizeOperand<'tcx>, Vec<Statement<'tcx>>)> {
        let src_ty = src.ty(&body.local_decls, tcx).ty;
        if !self.alias_exempt_root_for_ty(tcx, body, src_ty) {
            return None;
        }

        match src.projection.last()? {
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {}
            _ => return None,
        }

        let base_place = PlaceRef {
            local: src.local,
            projection: &src.projection[..src.projection.len() - 1],
        }
        .to_place(tcx);
        let base_ty = base_place.ty(&body.local_decls, tcx).ty;
        let TyKind::Array(elem_ty, _) = base_ty.kind() else {
            return None;
        };
        if *elem_ty != src_ty {
            return None;
        }

        let extent_addr_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.usize, source_info.span));
        let (extent_addr_stmt1, extent_addr_stmt2) = self.slot_addr_stmts_for_place(
            tcx,
            body,
            source_info,
            base_place,
            extent_addr_local,
            false,
        )?;
        let extent_len = self.size_operand_for_ty(tcx, body, base_ty, source_info.span);

        Some((
            Operand::Copy(Place::from(extent_addr_local)),
            extent_len,
            vec![extent_addr_stmt1, extent_addr_stmt2],
        ))
    }

    pub(in crate::instrumentation) fn tb_call_arg_protector_supported_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, Mutability::Mut) => pointee.is_unpin(tcx, body.typing_env(tcx)),
            _ => true,
        }
    }

    /// Preserve alias exemption for direct derivations out of an interior-mutability root
    /// (`UnsafeCell<T>`, `Cell<T>`, etc.) when the child pointee still fits entirely within
    /// the source storage.
    ///
    /// This keeps wrappers like `UnsafeCell::get()` exempt, while avoiding false negatives
    /// when a non-root aggregate or a zero-sized interior-mutable wrapper is cast to some
    /// unrelated byte/field pointer.
    pub(in crate::instrumentation) fn alias_exempt_child_from_source_ptr<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_ty: Ty<'tcx>,
        dst_ptr_ty: Ty<'tcx>,
    ) -> bool {
        let src_pointee = match src_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return false,
        };
        let dst_pointee = match dst_ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => *pointee,
            _ => return false,
        };

        if !self.alias_exempt_root_for_ty(tcx, body, src_pointee) {
            return false;
        }

        let src_size = self.layout_size_bytes(tcx, src_pointee);
        let dst_size = self.layout_size_bytes(tcx, dst_pointee);
        src_size != 0 && dst_size != 0 && dst_size <= src_size
    }

    /// Return the alias-exempt classification for the memory location accessed by `ty`.
    ///
    /// For pointer-typed access places (`&T`, `*mut T`) the access is to the pointee, not to the
    /// pointer value itself. For projected writes like `(*p).field = ...`, `ty` is already the
    /// field type, which lets us keep interior-mutable fields alias-exempt without exempting the
    /// entire aggregate.
    pub(in crate::instrumentation) fn alias_exempt_for_access_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                self.alias_exempt_for_ty(tcx, body, *pointee)
            }
            _ => self.alias_exempt_for_ty(tcx, body, ty),
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

    pub(in crate::instrumentation) fn type_needs_normalization<'tcx>(&self, ty: Ty<'tcx>) -> bool {
        struct NeedsNormalizationVisitor;

        impl<'tcx> TypeVisitor<TyCtxt<'tcx>> for NeedsNormalizationVisitor {
            type Result = ControlFlow<()>;

            fn visit_ty(&mut self, ty: Ty<'tcx>) -> Self::Result {
                match ty.kind() {
                    TyKind::Alias(..)
                    | TyKind::Param(..)
                    | TyKind::Bound(..)
                    | TyKind::Placeholder(..)
                    | TyKind::Infer(..)
                    | TyKind::Error(..) => ControlFlow::Break(()),
                    _ => ty.super_visit_with(self),
                }
            }

            fn visit_const(&mut self, c: rustc_middle::ty::Const<'tcx>) -> Self::Result {
                match c.kind() {
                    TyConstKind::Param(..)
                    | TyConstKind::Infer(..)
                    | TyConstKind::Bound(..)
                    | TyConstKind::Placeholder(..)
                    | TyConstKind::Unevaluated(..)
                    | TyConstKind::Expr(..)
                    | TyConstKind::Error(..) => ControlFlow::Break(()),
                    _ => c.super_visit_with(self),
                }
            }
        }

        let mut v = NeedsNormalizationVisitor;
        ty.visit_with(&mut v).is_break()
    }

    pub(in crate::instrumentation) fn mir_const_needs_normalization<'tcx>(
        &self,
        c: rustc_middle::mir::Const<'tcx>,
    ) -> bool {
        struct NeedsNormalizationVisitor;

        impl<'tcx> TypeVisitor<TyCtxt<'tcx>> for NeedsNormalizationVisitor {
            type Result = ControlFlow<()>;

            fn visit_ty(&mut self, ty: Ty<'tcx>) -> Self::Result {
                match ty.kind() {
                    TyKind::Alias(..)
                    | TyKind::Param(..)
                    | TyKind::Bound(..)
                    | TyKind::Placeholder(..)
                    | TyKind::Infer(..)
                    | TyKind::Error(..) => ControlFlow::Break(()),
                    _ => ty.super_visit_with(self),
                }
            }

            fn visit_const(&mut self, c: rustc_middle::ty::Const<'tcx>) -> Self::Result {
                match c.kind() {
                    TyConstKind::Param(..)
                    | TyConstKind::Infer(..)
                    | TyConstKind::Bound(..)
                    | TyConstKind::Placeholder(..)
                    | TyConstKind::Unevaluated(..)
                    | TyConstKind::Expr(..)
                    | TyConstKind::Error(..) => ControlFlow::Break(()),
                    _ => c.super_visit_with(self),
                }
            }
        }

        if matches!(c, rustc_middle::mir::Const::Unevaluated(..)) {
            return true;
        }

        let mut v = NeedsNormalizationVisitor;
        c.visit_with(&mut v).is_break()
    }

    // Resolve const/promoted pointers to their global allocation metadata (size + offset).
    pub(in crate::instrumentation) fn const_alloc_info<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        c: &ConstOperand<'tcx>,
    ) -> Option<ConstAllocInfo> {
        let const_ty = c.const_.ty();
        if const_ty.has_param()
            || const_ty.has_infer()
            || const_ty.has_aliases()
            || const_ty.has_opaque_types()
            || const_ty.has_placeholders()
            || const_ty.has_bound_vars()
            || self.type_needs_normalization(const_ty)
            || self.mir_const_needs_normalization(c.const_)
        {
            return None;
        }

        // Some dependency graphs (for example `mail-internals` via `object`) still trigger
        // rustc normalization ICEs while evaluating pointer-valued MIR constants with
        // projection-heavy associated types. Unknown const allocation info is acceptable for our
        // instrumentation, so treat those consts conservatively instead of crashing the compiler.
        let scalar = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.const_
                .try_eval_scalar(tcx, TypingEnv::fully_monomorphized())
        }))
        .ok()
        .flatten()?;
        let ptr = scalar.to_pointer(&tcx).discard_err()?;
        let (prov_opt, offset) = ptr.into_raw_parts();
        let prov = prov_opt?;
        let alloc_id = prov.alloc_id();

        let size = match tcx.global_alloc(alloc_id) {
            GlobalAlloc::Memory(mem) => mem.inner().size().bytes() as usize,
            GlobalAlloc::Static(def_id) => {
                let ty = tcx.type_of(def_id).skip_binder();
                // Same issue as above: statics can have types that still require
                // normalization/projection evaluation in ways that can ICE/error.
                // Unknown is fine.
                if ty.has_param()
                    || ty.has_infer()
                    || ty.has_aliases()
                    || ty.has_opaque_types()
                    || ty.has_placeholders()
                {
                    0
                } else {
                    self.layout_size_bytes(tcx, ty)
                }
            }
            _ => return None,
        };

        Some(ConstAllocInfo {
            size,
            base_offset: offset.bytes() as usize,
        })
    }
}
