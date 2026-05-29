use super::*;

impl MyOptimizationPass {
    /// Return whether `local` has an explicit slot-family channel.
    ///
    /// Unlike `ty_contains_direct_pointer_fields`, this is allowed to recurse through owner or
    /// container internals. The slot-family models the borrow family of the outer slot `T`
    /// itself, not the ABI transport of any specific nested raw field. Types like `BytesMut`
    /// therefore need this path even though they do not store a source-level reference/raw
    /// directly.
    pub(in crate::instrumentation) fn supports_slot_family_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        let local_ty = body.local_decls[local].ty;
        if !local_ty.is_sized(tcx, body.typing_env(tcx)) {
            return false;
        }
        if self.is_pointer_ty(local_ty) {
            return false;
        }
        if !self.ty_contains_pointer_fields(
            tcx,
            body,
            local_ty,
            SHADOWABLE_LEAF_PTR_RECURSION_DEPTH,
        ) {
            return false;
        }

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
        self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty)
    }

    /// Return whether `local` should participate in ref-style by-value carrier transport.
    ///
    /// This is intentionally narrower than `supports_arg_anchor_take_local`: only values that
    /// structurally carry a source-level reference qualify. Raw-owner/container aggregates like
    /// `BytesMut`, `Vec`, `Box`, `RawVec`, or `NonNull`-based wrappers are excluded here because
    /// transporting them as slot-level references creates false positives at ordinary by-value
    /// call boundaries.
    pub(in crate::instrumentation) fn supports_call_boundary_anchor_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if !self.supports_slot_family_local(tcx, body, local) {
            return false;
        }
        let local_ty = body.local_decls[local].ty;
        if !self.ty_contains_direct_ref_fields(tcx, local_ty) {
            return false;
        }

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
        self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty)
    }

    /// Return whether `local` needs slot-family anchor transport at by-value call boundaries.
    ///
    /// This is broader than ref-style anchor transport: raw-owner/container aggregates do not
    /// carry a source-level `&T`, but callees may still create helper borrows from the moved
    /// carrier. Those helper borrows should inherit the caller-visible carrier family.
    pub(in crate::instrumentation) fn supports_call_boundary_slot_anchor_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if !self.supports_slot_family_local(tcx, body, local) {
            return false;
        }
        let local_ty = body.local_decls[local].ty;
        if self.is_pointer_ty(local_ty)
            || !self.ty_contains_pointer_fields(
                tcx,
                body,
                local_ty,
                SHADOWABLE_LEAF_PTR_RECURSION_DEPTH,
            )
        {
            return false;
        }

        let raw_ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
        self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty)
    }

    /// Return whether `ty` should use structural leaf-shadow transport at by-value call
    /// boundaries.
    ///
    /// This is the raw-owner/container path: the value carries pointer bytes that should survive
    /// the ABI move into the callee, but it does not itself represent a source-level borrow
    /// carrier.
    pub(in crate::instrumentation) fn supports_call_boundary_leaf_shadow_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        !self.is_pointer_ty(ty)
            && self.ty_contains_pointer_fields(tcx, body, ty, SHADOWABLE_LEAF_PTR_RECURSION_DEPTH)
            && !self.ty_contains_direct_ref_fields(tcx, ty)
    }

    pub(in crate::instrumentation) fn supports_call_boundary_leaf_shadow_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if !self.supports_slot_family_local(tcx, body, local) {
            return false;
        }
        self.supports_call_boundary_leaf_shadow_ty(tcx, body, body.local_decls[local].ty)
    }

    /// Return whether a by-value carrier can rebuild its local slot-family anchor from one
    /// transported pointer leaf shadow instead of transporting a separate whole-slot boundary tag.
    pub(in crate::instrumentation) fn supports_call_boundary_leaf_seeded_anchor_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if !self.supports_call_boundary_leaf_shadow_local(tcx, body, local) {
            return false;
        }
        self.shadowable_leaf_ptr_specs_from_place(
            tcx,
            body,
            Place::from(local),
            body.local_decls[local].ty,
        )
        .len()
            == 1
    }

    /// Return whether a by-value carrier still needs a dedicated whole-slot boundary tag.
    pub(in crate::instrumentation) fn supports_call_boundary_whole_slot_anchor_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        self.supports_call_boundary_slot_anchor_local(tcx, body, local)
            && !self.supports_call_boundary_leaf_seeded_anchor_local(tcx, body, local)
    }

    pub(in crate::instrumentation) fn is_whole_place_slot_family_source<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
    ) -> bool {
        src_place.projection.is_empty()
            && matches!(
                self.creation_parent_selection_mode_for_src_place(
                    tcx, body, src_place, false, true,
                ),
                ParentSelectionMode::SlotFamily
            )
    }

    pub(in crate::instrumentation) fn pair_shadowable_leaf_ptr_specs<'tcx>(
        &self,
        dst_specs: &[ShadowableLeafPtrSpec<'tcx>],
        src_specs: &[ShadowableLeafPtrSpec<'tcx>],
    ) -> Option<Vec<(ShadowableLeafPtrSpec<'tcx>, ShadowableLeafPtrSpec<'tcx>)>> {
        if dst_specs.is_empty() || dst_specs.len() != src_specs.len() {
            return None;
        }

        if dst_specs.iter().all(|spec| spec.byte_offset.is_some())
            && src_specs.iter().all(|spec| spec.byte_offset.is_some())
        {
            let mut src_by_offset: HashMap<u64, ShadowableLeafPtrSpec<'tcx>> = HashMap::new();
            for src_spec in src_specs.iter().copied() {
                let offset = src_spec.byte_offset.expect("checked above");
                if src_by_offset.insert(offset, src_spec).is_some() {
                    src_by_offset.clear();
                    break;
                }
            }
            if !src_by_offset.is_empty() {
                let mut pairs = Vec::with_capacity(dst_specs.len());
                let mut matched_all = true;
                for dst_spec in dst_specs.iter().copied() {
                    let offset = dst_spec.byte_offset.expect("checked above");
                    let Some(src_spec) = src_by_offset.remove(&offset) else {
                        matched_all = false;
                        break;
                    };
                    pairs.push((dst_spec, src_spec));
                }
                if matched_all && src_by_offset.is_empty() {
                    return Some(pairs);
                }
            }
        }

        let mut src_by_key: HashMap<u64, ShadowableLeafPtrSpec<'tcx>> = HashMap::new();
        let mut duplicate_key = false;
        for src_spec in src_specs.iter().copied() {
            if src_by_key
                .insert(src_spec.transport_key(), src_spec)
                .is_some()
            {
                duplicate_key = true;
                break;
            }
        }
        if !duplicate_key {
            let mut pairs = Vec::with_capacity(dst_specs.len());
            let mut matched_all = true;
            for dst_spec in dst_specs.iter().copied() {
                let Some(src_spec) = src_by_key.remove(&dst_spec.transport_key()) else {
                    matched_all = false;
                    break;
                };
                pairs.push((dst_spec, src_spec));
            }
            if matched_all && src_by_key.is_empty() {
                return Some(pairs);
            }
        }

        None
    }

    /// Pair return-value shadow leaves with the pointer/view provenance of `arg0`.
    ///
    /// This handles view constructors like `split_at{,_mut}` and `VecDeque::as_slices`, where
    /// one pointer-bearing input produces an aggregate return with multiple pointer/view leaves.
    /// We first try the normal 1:1 structural pairing. If that fails and `arg0` contributes a
    /// single shadowable leaf, we fan that one source leaf out to every returned leaf.
    pub(in crate::instrumentation) fn pair_shadowable_leaf_ptr_specs_from_arg0<'tcx>(
        &self,
        dst_specs: &[ShadowableLeafPtrSpec<'tcx>],
        src_specs: &[ShadowableLeafPtrSpec<'tcx>],
    ) -> Option<Vec<(ShadowableLeafPtrSpec<'tcx>, ShadowableLeafPtrSpec<'tcx>)>> {
        if let Some(pairs) = self.pair_shadowable_leaf_ptr_specs(dst_specs, src_specs) {
            return Some(pairs);
        }

        if dst_specs.is_empty() || src_specs.len() != 1 {
            return None;
        }

        let src_spec = src_specs[0];
        Some(
            dst_specs
                .iter()
                .copied()
                .map(|dst_spec| (dst_spec, src_spec))
                .collect(),
        )
    }
}

/// Pair returned aggregate pointer leaves with the structural source leaves of arg0.
///
/// If arg0 is a reference to a carrier, prefer leaves inside the pointee. For example,
/// `clone_like(&bytes) -> Bytes` should copy the returned `Bytes.ptr` shadow from
/// `(*arg0).ptr`, not from the `&Bytes` receiver tag.
pub(in crate::instrumentation) fn pair_return_leafs_from_arg0<'tcx>(
    pass: &MyOptimizationPass,
    dst_leafs: &[ShadowableLeafPtrSpec<'tcx>],
    direct_src_leafs: &[ShadowableLeafPtrSpec<'tcx>],
    pointee_src_leafs: Option<&[ShadowableLeafPtrSpec<'tcx>]>,
) -> Option<Vec<(ShadowableLeafPtrSpec<'tcx>, ShadowableLeafPtrSpec<'tcx>)>> {
    if let Some(pointee_src_leafs) = pointee_src_leafs.filter(|leafs| !leafs.is_empty()) {
        if let Some(pairs) =
            pass.pair_shadowable_leaf_ptr_specs_from_arg0(dst_leafs, pointee_src_leafs)
        {
            return Some(pairs);
        }
    }

    pass.pair_shadowable_leaf_ptr_specs_from_arg0(dst_leafs, direct_src_leafs)
}
