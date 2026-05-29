use super::super::*;

impl MyOptimizationPass {
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
