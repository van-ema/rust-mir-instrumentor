//! Detects types that should be exempt from alias checks.

use super::super::*;

impl MyOptimizationPass {
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
                    || path.contains("::sync::atomic::Atomic")
                    || path.contains("::pin::UnsafePinned")
            }
            TyKind::Tuple(_) | TyKind::Array(..) | TyKind::Slice(_) => false,
            _ => true,
        }
    }

    fn is_interior_mut_root_adt_path(path: &str) -> bool {
        path.contains("::cell::UnsafeCell")
            || path.contains("::cell::SyncUnsafeCell")
            || path.contains("::cell::Cell")
            || path.contains("::cell::RefCell")
            || path.contains("::sync::atomic::Atomic")
            || path.contains("::pin::UnsafePinned")
    }

    pub(in crate::instrumentation) fn known_interior_mut_root_for_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        if matches!(ty.kind(), TyKind::Slice(_) | TyKind::Str) {
            return false;
        }

        if let TyKind::Adt(adt, _) = ty.kind() {
            return Self::is_interior_mut_root_adt_path(&tcx.def_path_str(adt.did()));
        }

        if ty.has_param()
            || ty.has_infer()
            || ty.has_aliases()
            || ty.has_opaque_types()
            || ty.has_placeholders()
        {
            return false;
        }

        let typing_env = body.typing_env(tcx);
        !ty.is_freeze(tcx, typing_env)
    }

    pub(in crate::instrumentation) fn known_interior_mut_root_for_ptr_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_ty: Ty<'tcx>,
    ) -> bool {
        match ptr_ty.kind() {
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                self.known_interior_mut_root_for_ty(tcx, body, *pointee)
            }
            _ => false,
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

    pub(in crate::instrumentation) fn interior_mut_extent_for_source_place<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        src: Place<'tcx>,
    ) -> Option<(Operand<'tcx>, SizeOperand<'tcx>, Vec<Statement<'tcx>>)> {
        let src_ty = src.ty(&body.local_decls, tcx).ty;
        if !self.known_interior_mut_root_for_ty(tcx, body, src_ty) {
            return None;
        }

        if matches!(
            src.projection.last(),
            Some(ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. })
        ) {
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

            return Some((
                Operand::Copy(Place::from(extent_addr_local)),
                extent_len,
                vec![extent_addr_stmt1, extent_addr_stmt2],
            ));
        }

        // Tree Borrows grants a shared reference to an interior-mutable field
        // permission for surrounding bytes in the containing aggregate. Carry
        // that immediate aggregate extent just as we already do for an indexed
        // interior-mutable array element.
        if matches!(src.projection.last(), Some(ProjectionElem::Field(_, _))) {
            let base_place = PlaceRef {
                local: src.local,
                projection: &src.projection[..src.projection.len() - 1],
            }
            .to_place(tcx);
            let base_ty = base_place.ty(&body.local_decls, tcx).ty;
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

            return Some((
                Operand::Copy(Place::from(extent_addr_local)),
                extent_len,
                vec![extent_addr_stmt1, extent_addr_stmt2],
            ));
        }

        let extent_addr_local = body
            .local_decls
            .push(LocalDecl::new(tcx.types.usize, source_info.span));
        let (extent_addr_stmt1, extent_addr_stmt2) =
            self.slot_addr_stmts_for_place(tcx, body, source_info, src, extent_addr_local, false)?;
        let extent_len = self.size_operand_for_ty(tcx, body, src_ty, source_info.span);

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
}
