use super::*;

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
