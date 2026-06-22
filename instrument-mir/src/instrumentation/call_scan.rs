//! Scans call terminators and models call-side pointer effects.

use super::*;

impl MyOptimizationPass {
    fn trace_call_boundary_scan_enabled(&self) -> bool {
        std::env::var("RZ_TRACE_CALL_BOUNDARY_SCAN")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    fn is_fn_trait_call<'tcx>(&self, tcx: TyCtxt<'tcx>, callee_def_id: DefId) -> bool {
        let Some(trait_id) = tcx.trait_of_assoc(callee_def_id) else {
            return false;
        };
        let lang_items = tcx.lang_items();
        [LangItem::Fn, LangItem::FnMut, LangItem::FnOnce]
            .into_iter()
            .any(|item| lang_items.get(item) == Some(trait_id))
    }

    fn call_arg_leaf_push_kind<'tcx>(
        direct_callee_id: Option<u64>,
        arg_index: usize,
        leaf_key: u64,
    ) -> InstrKind<'tcx> {
        if let Some(callee_id) = direct_callee_id {
            InstrKind::CallArgLeafPush {
                callee_id,
                arg_index: arg_index as u64,
                leaf_key,
            }
        } else {
            InstrKind::IndirectCallArgLeafPush {
                arg_index: arg_index as u64,
                leaf_key,
            }
        }
    }

    fn push_call_arg_leaf_insert<'tcx>(
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        direct_callee_id: Option<u64>,
        arg_index: usize,
        leaf_spec: ShadowableLeafPtrSpec<'tcx>,
    ) {
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before: false,
            source_info,
            place: leaf_spec.place,
            kind: Self::call_arg_leaf_push_kind(
                direct_callee_id,
                arg_index,
                leaf_spec.call_boundary_key(),
            ),
        });
    }

    fn push_direct_call_arg_leaf_insert<'tcx>(
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        callee_id: u64,
        arg_index: usize,
        leaf_spec: ShadowableLeafPtrSpec<'tcx>,
    ) {
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before: false,
            source_info,
            place: leaf_spec.place,
            kind: InstrKind::CallArgLeafPush {
                callee_id,
                arg_index: arg_index as u64,
                leaf_key: leaf_spec.call_boundary_key(),
            },
        });
    }

    fn push_ret_leaf_take_inserts<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        callee_id: u64,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
    ) {
        for dst_leaf_spec in
            self.call_boundary_leaf_ptr_specs_from_place(tcx, body, Place::from(dst_local), dst_ty)
        {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx,
                insert_before: false,
                source_info,
                place: dst_leaf_spec.place,
                kind: InstrKind::RetLeafTake {
                    callee_id,
                    leaf_key: dst_leaf_spec.call_boundary_key(),
                },
            });
        }
    }

    /// Best-effort check: does this span come from the Rust std/core/alloc sources?
    /// This is used to suppress noisy PtrUse hooks for std wrappers (e.g. println!).
    pub(in crate::instrumentation) fn span_is_stdlib<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        span: Span,
    ) -> bool {
        let sm = tcx.sess.source_map();
        let filename = sm.span_to_filename(span);
        // `FileName` is not `Display` on this nightly; use `Debug` formatting.
        let s = format!("{:?}", filename);

        // Matches typical rustup toolchain paths and in-tree paths.
        s.contains("/lib/rustlib/src/rust/library/std/")
            || s.contains("/lib/rustlib/src/rust/library/core/")
            || s.contains("/lib/rustlib/src/rust/library/alloc/")
            || s.contains("/rust/library/std/")
            || s.contains("/rust/library/core/")
            || s.contains("/rust/library/alloc/")
            || s.contains("/rust/library/proc_macro/")
            || s.contains("/library/std/")
            || s.contains("/library/core/")
            || s.contains("/library/alloc/")
    }

    /// Byte size for `ptr::copy` / `copy_nonoverlapping` / `write_bytes` effects.
    ///
    /// This is an access length, not tag-bounds metadata. If the pointee size is not known, use
    /// `0` so the runtime treats the access length as unknown; never pass the bounds sentinel
    /// (`usize::MAX`) as a real read/write size.
    pub(in crate::instrumentation) fn memop_access_size_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        count_op: &Operand<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        let elem_ty = match ptr_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => *pointee_ty,
            TyKind::Ref(_, pointee_ty, _) => *pointee_ty,
            _ => {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
        };

        if !elem_ty.is_sized(tcx, body.typing_env(tcx)) {
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }

        SizeOperand::ElemCount {
            elem_ty,
            count_op: count_op.clone(),
        }
    }

    /// Recognize std/alloc Box wrappers that return a raw pointer but take an ADT (Box<T>) as input.
    ///
    /// In optimized MIR, `Box::into_raw` appears as a direct call where the argument is an ADT,
    /// so we cannot use TagProp (arg0 is not a thin pointer local). We therefore synthesize a root
    /// raw-pointer tag for the returned pointer local.
    pub(in crate::instrumentation) fn is_box_into_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::into_raw")
    }

    pub(in crate::instrumentation) fn is_box_new_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.ends_with("::new")
    }

    pub(in crate::instrumentation) fn is_box_ty<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        ty: Ty<'tcx>,
    ) -> bool {
        match ty.kind() {
            TyKind::Adt(adt, _) => {
                let def_path = tcx.def_path_str(adt.did());
                def_path.contains("::boxed::Box") || def_path.contains("boxed::Box")
            }
            _ => false,
        }
    }

    pub(in crate::instrumentation) fn is_box_from_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::from_raw")
    }

    pub(in crate::instrumentation) fn is_std_fs_read_fn(&self, def_path: &str) -> bool {
        def_path == "std::fs::read"
    }

    pub(in crate::instrumentation) fn is_result_unwrap_or_expect_fn(&self, def_path: &str) -> bool {
        def_path.contains("::result::Result")
            && (def_path.ends_with("::unwrap") || def_path.ends_with("::expect"))
    }

    pub(in crate::instrumentation) fn local_is_std_fs_read_result<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        for block_data in body.basic_blocks.iter() {
            let Some(term) = &block_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                func, destination, ..
            } = &term.kind
            else {
                continue;
            };
            if destination.as_local() != Some(local) {
                continue;
            }
            let Some((did, _)) = self.direct_callee(tcx, body, block_data, func) else {
                continue;
            };
            if self.is_std_fs_read_fn(&tcx.def_path_str(did)) {
                return true;
            }
        }
        false
    }

    pub(in crate::instrumentation) fn is_std_fs_read_ok_vec_payload<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        src_place: Place<'tcx>,
        dst_ty: Ty<'tcx>,
    ) -> bool {
        if !self.is_vec_u8_ty(tcx, dst_ty) || src_place.projection.len() != 2 {
            return false;
        }

        let ProjectionElem::Downcast(_, variant_idx) = src_place.projection[0] else {
            return false;
        };
        let ProjectionElem::Field(field_idx, field_ty) = src_place.projection[1] else {
            return false;
        };
        if field_idx.index() != 0 || field_ty != dst_ty {
            return false;
        }

        let src_ty = body.local_decls[src_place.local].ty;
        let TyKind::Adt(adt, args) = src_ty.kind() else {
            return false;
        };
        if !tcx.def_path_str(adt.did()).contains("::result::Result") {
            return false;
        }
        let variant = adt.variant(variant_idx);
        if variant.name.as_str() != "Ok" || variant.fields[field_idx].ty(tcx, args) != dst_ty {
            return false;
        }

        self.local_is_std_fs_read_result(tcx, body, src_place.local)
    }

    pub(in crate::instrumentation) fn call_returns_std_fs_read_vec_payload<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        callee_path: Option<&str>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        dst_ty: Ty<'tcx>,
    ) -> bool {
        if !self.is_vec_u8_ty(tcx, dst_ty)
            || !callee_path.is_some_and(|path| self.is_result_unwrap_or_expect_fn(path))
        {
            return false;
        }
        let Some(src_place) = args
            .get(0)
            .and_then(|arg| self.place_from_operand(&arg.node))
        else {
            return false;
        };
        src_place.projection.is_empty()
            && self.local_is_std_fs_read_result(tcx, body, src_place.local)
    }

    pub(in crate::instrumentation) fn push_external_vec_u8_owner_import<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        insert_before: bool,
        source_info: SourceInfo,
        dst_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) -> bool {
        let dst_ty = body.local_decls[dst_local].ty;
        if !self.is_vec_u8_ty(tcx, dst_ty) {
            return false;
        }
        let leafs =
            self.shadowable_leaf_ptr_specs_from_place(tcx, body, Place::from(dst_local), dst_ty);
        let [leaf] = leafs.as_slice() else {
            return false;
        };
        insert_points.push(InsertPoint {
            bb,
            stmt_idx,
            insert_before,
            source_info,
            place: leaf.place,
            kind: InstrKind::ShadowStoreExternalAllocRoot { is_mut: true },
        });
        true
    }

    pub(in crate::instrumentation) fn direct_callee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        func: &Operand<'tcx>,
    ) -> Option<(DefId, u64)> {
        let mut def_id_opt: Option<DefId> = None;

        if let TyKind::FnDef(callee_def_id, args) = func.ty(body, tcx).kind() {
            def_id_opt = Some(self.resolve_instance_def_id(tcx, body, *callee_def_id, args));
        } else if let Operand::Constant(c) = func {
            // Some direct calls come through a function pointer constant.
            def_id_opt = self.const_fn_def_id(tcx, body, c);
        } else if let Operand::Copy(p) | Operand::Move(p) = func {
            let ty = p.ty(&body.local_decls, tcx).ty;
            if let TyKind::FnDef(callee_def_id, args) = ty.kind() {
                def_id_opt = Some(self.resolve_instance_def_id(tcx, body, *callee_def_id, args));
            } else if matches!(ty.kind(), TyKind::FnPtr(..)) {
                def_id_opt = self.backtrack_fn_ptr_def_id(tcx, body, block_data, p.local);
            }
        }

        def_id_opt.map(|def_id| (def_id, self.callee_id_u64(tcx, def_id)))
    }

    /// Best-effort: recover the pointer source local from a call argument.
    ///
    /// Most pointer-derivation wrappers carry provenance in arg0, but some trait-based helpers
    /// (notably `SliceIndex::index{,_mut}`) take the pointer-bearing slice in arg1 and use arg0
    /// for an index/range value.

    pub(in crate::instrumentation) fn call_arg_pointer_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        arg_index: usize,
    ) -> Option<Local> {
        let first = args.get(arg_index)?;
        let arg_place = self.place_from_operand(&first.node)?;
        let arg_local = arg_place.local;
        let arg_ty = body.local_decls[arg_local].ty;
        if self.is_pointer_ty(arg_ty) {
            return Some(arg_local);
        }
        if !arg_place.projection.is_empty() {
            for prefix_len in (0..arg_place.projection.len()).rev() {
                let prefix = PlaceRef {
                    local: arg_place.local,
                    projection: &arg_place.projection[..prefix_len],
                }
                .to_place(tcx);
                let prefix_ty = prefix.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(prefix_ty) {
                    return Some(prefix.local);
                }
            }
        }
        if let Some(src_local) =
            self.backtrack_pointer_source_local(body, arg_local, &block_data.statements)
        {
            return Some(src_local);
        }
        let base_local = self.backtrack_unsize_base_local(arg_local, &block_data.statements)?;
        if self.is_pointer_ty(body.local_decls[base_local].ty) {
            Some(base_local)
        } else {
            None
        }
    }

    pub(in crate::instrumentation) fn call_arg_lineage_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        arg_index: usize,
    ) -> Option<Local> {
        if let Some(src_local) =
            self.call_arg_pointer_source_local(tcx, body, block_data, args, arg_index)
        {
            return Some(src_local);
        }

        let first = args.get(arg_index)?;
        let arg_place = self.place_from_operand(&first.node)?;
        if arg_place.projection.is_empty() {
            Some(arg_place.local)
        } else {
            None
        }
    }

    pub(in crate::instrumentation) fn backtrack_same_typed_call_result_source_local<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        agg_local: Local,
        wanted_ty: Ty<'tcx>,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for pred_bb in body.basic_blocks.indices() {
            let pred_data = &body.basic_blocks[pred_bb];
            let Some(term) = &pred_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args, destination, ..
            } = &term.kind
            else {
                continue;
            };

            if destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut candidates: HashSet<Local> = HashSet::new();
            for arg in args.iter() {
                let Some(arg_place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                if !arg_place.projection.is_empty() {
                    continue;
                }
                let arg_ty = arg_place.ty(&body.local_decls, tcx).ty;
                if arg_ty != wanted_ty || self.is_pointer_ty(arg_ty) {
                    continue;
                }
                if !self.supports_slot_family_local(tcx, body, arg_place.local) {
                    continue;
                }
                candidates.insert(arg_place.local);
                if candidates.len() > 1 {
                    return None;
                }
            }

            let Some(src_local) = candidates.into_iter().next() else {
                return None;
            };

            match recovered {
                Some(existing) if existing != src_local => return None,
                Some(_) => {}
                None => recovered = Some(src_local),
            }
        }

        if matched_call {
            recovered
        } else {
            None
        }
    }

    /// Recover a pointer lineage source when a call writes an aggregate result into `agg_local`
    /// and a successor block later extracts a pointer field from that aggregate.
    ///
    /// Narrow shape handled:
    /// - predecessor terminator is a call whose `destination.local == agg_local`
    /// - call target is the current block
    /// - among the call arguments there is exactly one recoverable pointer source local
    ///
    /// This covers wrappers like `Result<&T, E>` where MIR stores the aggregate result in a
    /// non-pointer local and a later `_dst = move ((_ret as Ok).0)` would otherwise lose the
    /// original parent lineage and fall back to `RawRoot`.

    pub(in crate::instrumentation) fn backtrack_single_pointer_arg_call_result_source_local<
        'tcx,
    >(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        agg_local: Local,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for pred_bb in body.basic_blocks.indices() {
            let pred_data = &body.basic_blocks[pred_bb];
            let Some(term) = &pred_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args,
                destination,
                target,
                ..
            } = &term.kind
            else {
                continue;
            };

            if *target != Some(bb) || destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut ptr_sources = HashSet::new();
            for arg_index in 0..args.len() {
                if let Some(src_local) =
                    self.call_arg_pointer_source_local(tcx, body, pred_data, args, arg_index)
                {
                    ptr_sources.insert(src_local);
                    if ptr_sources.len() > 1 {
                        return None;
                    }
                }
            }

            let Some(src_local) = ptr_sources.into_iter().next() else {
                return None;
            };

            match recovered {
                Some(existing) if existing != src_local => return None,
                Some(_) => {}
                None => recovered = Some(src_local),
            }
        }

        if matched_call {
            recovered
        } else {
            None
        }
    }

    /// Conservative global recovery for aggregate locals produced by a call result where the
    /// callee has exactly one recoverable pointer source argument across all definitions.
    ///
    /// This is the fallback needed for patterns like:
    /// - predecessor block: `_agg = iter.next()`
    /// - successor block: `_val = copy (((_agg as Some).0).1)`
    ///
    /// The extraction block is not necessarily the direct call target, so
    /// `backtrack_single_pointer_arg_call_result_source_local` can miss it.

    pub(in crate::instrumentation) fn backtrack_global_pointer_arg_call_result_source_local<
        'tcx,
    >(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        agg_local: Local,
    ) -> Option<Local> {
        let mut recovered: Option<Local> = None;
        let mut matched_call = false;

        for block_data in body.basic_blocks.iter() {
            let Some(term) = &block_data.terminator else {
                continue;
            };
            let TerminatorKind::Call {
                args, destination, ..
            } = &term.kind
            else {
                continue;
            };
            if destination.local != agg_local {
                continue;
            }
            matched_call = true;

            let mut ptr_sources = HashSet::new();
            for arg_index in 0..args.len() {
                if let Some(src_local) =
                    self.call_arg_pointer_source_local(tcx, body, block_data, args, arg_index)
                {
                    ptr_sources.insert(src_local);
                    if ptr_sources.len() > 1 {
                        return None;
                    }
                }
            }

            let Some(src_local) = ptr_sources.into_iter().next() else {
                return None;
            };

            match recovered {
                Some(existing) if existing != src_local => return None,
                Some(_) => {}
                None => recovered = Some(src_local),
            }
        }

        if matched_call {
            recovered
        } else {
            None
        }
    }

    pub(in crate::instrumentation) fn ptr_derive_source_arg_index(&self, def_path: &str) -> usize {
        if def_path.contains("SliceIndex")
            && (def_path.ends_with("::index") || def_path.ends_with("::index_mut"))
        {
            1
        } else if def_path.contains("::slice::index::<impl")
            && (def_path.ends_with("::index") || def_path.ends_with("::index_mut"))
        {
            1
        } else {
            0
        }
    }

    pub(in crate::instrumentation) fn ptr_derive_call_requires_strict_validation(
        &self,
        def_path: &str,
    ) -> bool {
        def_path.contains("::ptr::")
            && (def_path.ends_with("::add")
                || def_path.ends_with("::sub")
                || def_path.ends_with("::offset")
                || def_path.ends_with("::byte_add")
                || def_path.ends_with("::byte_sub"))
    }

    pub(in crate::instrumentation) fn ptr_derive_call_is_wrapping(&self, def_path: &str) -> bool {
        def_path.contains("::ptr::")
            && (def_path.ends_with("::wrapping_add")
                || def_path.ends_with("::wrapping_sub")
                || def_path.ends_with("::wrapping_offset")
                || def_path.ends_with("::wrapping_byte_add")
                || def_path.ends_with("::wrapping_byte_sub"))
    }

    fn place_is_deref_of_local<'tcx>(&self, place: Place<'tcx>, local: Local) -> bool {
        place.local == local
            && place
                .projection
                .first()
                .is_some_and(|elem| matches!(elem, ProjectionElem::Deref))
    }

    fn stmt_uses_local_as_deref<'tcx>(&self, stmt: &Statement<'tcx>, local: Local) -> bool {
        let StatementKind::Assign(box (lhs, rhs)) = &stmt.kind else {
            return false;
        };
        if self.place_is_deref_of_local(*lhs, local) {
            return true;
        }
        match rhs {
            Rvalue::Use(Operand::Copy(p) | Operand::Move(p)) | Rvalue::CopyForDeref(p) => {
                self.place_is_deref_of_local(*p, local)
            }
            _ => false,
        }
    }

    fn stmt_overwrites_local<'tcx>(&self, stmt: &Statement<'tcx>, local: Local) -> bool {
        match &stmt.kind {
            StatementKind::Assign(box (place, _)) => {
                place.local == local && place.projection.is_empty()
            }
            StatementKind::StorageDead(dead) => *dead == local,
            _ => false,
        }
    }

    fn target_path_forces_deref_of_local<'tcx>(
        &self,
        body: &Body<'tcx>,
        start: BasicBlock,
        local: Local,
    ) -> bool {
        // Best-effort immediate-deref detector for wrapping pointer calls. Keep this narrow:
        // statement-level `*p` uses force eager validation, while call terminators such as
        // `ptr::read(p)` are modeled by their access/call effects and validate at the actual use.
        let mut current = start;
        for _ in 0..8 {
            let block = &body.basic_blocks[current];
            for stmt in &block.statements {
                if self.stmt_uses_local_as_deref(stmt, local) {
                    return true;
                }
                if self.stmt_overwrites_local(stmt, local) {
                    return false;
                }
            }

            let Some(term) = &block.terminator else {
                return false;
            };
            current = match &term.kind {
                TerminatorKind::Goto { target } => *target,
                TerminatorKind::Assert { target, .. } => *target,
                _ => return false,
            };
        }
        false
    }

    fn ptr_derive_call_strict_at_destination<'tcx>(
        &self,
        body: &Body<'tcx>,
        def_path: Option<&str>,
        call_target_bb: Option<BasicBlock>,
        dst_local: Local,
    ) -> bool {
        let Some(def_path) = def_path else {
            return false;
        };
        if self.ptr_derive_call_requires_strict_validation(def_path) {
            return true;
        }
        self.ptr_derive_call_is_wrapping(def_path)
            && call_target_bb.is_some_and(|target| {
                self.target_path_forces_deref_of_local(body, target, dst_local)
            })
    }

    pub(in crate::instrumentation) fn local_is_temp_like<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        if local == RETURN_PLACE || body.args_iter().any(|arg| arg == local) {
            return false;
        }

        if body.var_debug_info.iter().any(|info| {
            let VarDebugInfoContents::Place(place) = info.value else {
                return false;
            };
            place.projection.is_empty() && place.local == local
        }) {
            return false;
        }

        match body.local_decls[local].local_info.as_ref() {
            rustc_middle::mir::ClearCrossCrate::Set(info) => !matches!(
                &**info,
                rustc_middle::mir::LocalInfo::User(_)
                    | rustc_middle::mir::LocalInfo::StaticRef { .. }
                    | rustc_middle::mir::LocalInfo::ConstRef { .. }
            ),
            rustc_middle::mir::ClearCrossCrate::Clear => true,
        }
    }

    pub(in crate::instrumentation) fn local_has_explicit_storage<'tcx>(
        &self,
        body: &Body<'tcx>,
        local: Local,
    ) -> bool {
        body.basic_blocks.iter().any(|block_data| {
            block_data.statements.iter().any(|stmt| {
                matches!(
                    stmt.kind,
                    StatementKind::StorageLive(l) | StatementKind::StorageDead(l) if l == local
                )
            })
        })
    }

    pub(in crate::instrumentation) fn push_ptr_derive_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        src_local: Local,
        strict_validity: bool,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        tagged_ptr_locals: &mut HashSet<Local>,
        classified_derive_ptr_local: &mut Option<Local>,
    ) {
        // This call derives a new pointer from `src_local` (e.g. add/sub/offset/as_ptr).
        // We will emit a PtrDerive hook for the result, so suppress the redundant coarse PtrUse
        // for the base pointer argument.
        *classified_derive_ptr_local = Some(src_local);

        // Fresh tag derived from the base pointer tag.
        let is_mut = match dst_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        };
        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

        // NOTE: for ptr-derivation wrappers (add/sub/offset/...), the destination local
        // is only initialized *after* the call returns. We must therefore insert the PtrDerive
        // hook in the call's `target` block, not in the call block itself, otherwise we
        // expose provenance of an uninitialized local and record a garbage pointee address.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        tagged_ptr_locals.insert(dst_local);
        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        } else {
            // Fallback (should not happen for normal calls): keep the old placement.
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDerive {
                    dst: dst_local,
                    src: src_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        }
    }

    pub(in crate::instrumentation) fn push_ptr_derive_parent_call<'tcx>(
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        strict_validity: bool,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        let is_mut = match dst_ty.kind() {
            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
            _ => false,
        };
        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        tagged_ptr_locals.insert(dst_local);
        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDeriveParent {
                    dst: dst_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        } else {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: true,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::PtrDeriveParent {
                    dst: dst_local,
                    is_mut,
                    is_ref,
                    strict_validity,
                },
            });
        }
    }

    pub(in crate::instrumentation) fn push_box_into_raw_call<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        // Special-case: Box::into_raw returns a thin pointer derived from a Box ADT argument.
        // Since arg0 is not a thin pointer local, TagProp cannot apply; synthesize a root tag.
        let is_mut = self.ptr_is_mut(dst_ty);

        // Best-effort heap range recording for Box<T>: the raw pointer points to the T allocation.
        // TODO: hook real allocator shims/drop glue to get exact layout/size in general.
        let size_op: SizeOperand<'tcx> = match dst_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => {
                self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
            }
            TyKind::Ref(_, pointee_ty, _) => {
                self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
            }
            _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
        };

        // Insert after the call returns (in the call target block), so dst has the real value.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        if let Some(tgt_bb) = call_target_bb {
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::RawRoot {
                    ptr_local: dst_local,
                    is_mut,
                    exposed_provenance: false,
                },
            });
            insert_points.push(InsertPoint {
                bb: tgt_bb,
                stmt_idx: 0,
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::HeapAlloc {
                    ptr_local: dst_local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
        } else {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::RawRoot {
                    ptr_local: dst_local,
                    is_mut,
                    exposed_provenance: false,
                },
            });
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: Place::from(dst_local),
                kind: InstrKind::HeapAlloc {
                    ptr_local: dst_local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
        }
    }

    pub(in crate::instrumentation) fn warn_unknown_call_if_needed<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        callee_path_opt: Option<&str>,
        callee_instrumented: bool,
        call_effect_opt: Option<CallEffect>,
    ) {
        if !self.warn_unknown_calls_enabled() {
            return;
        }

        if let Some(def_path) = callee_path_opt {
            if !def_path.starts_with("core::") && !def_path.starts_with("std::") {
                return;
            }

            // Does the call take any pointer argument?
            let mut has_ptr_arg = false;
            for a in args.iter() {
                if let Some(p) = self.place_from_operand(&a.node) {
                    let ty = body.local_decls[p.local].ty;
                    if self.is_pointer_ty(ty) {
                        has_ptr_arg = true;
                        break;
                    }
                }
            }

            // Does the call return a pointer into a local?
            let returns_ptr = destination
                .as_local()
                .is_some_and(|dl| self.is_pointer_ty(body.local_decls[dl].ty));

            if (has_ptr_arg || returns_ptr) && !callee_instrumented {
                // Use the centralized classifier so warning suppression matches actual handling.
                let effect = call_effect_opt.unwrap_or_else(|| self.classify_call_effect(def_path));
                let known = !matches!(effect, CallEffect::Unknown);
                let trace_enabled = std::env::var("RZ_TRACE_CLASSIFY")
                    .ok()
                    .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
                    || self.log_enabled(PassLogLevel::Trace);
                if trace_enabled {
                    static TRACE_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    let filter = std::env::var("RZ_TRACE_CLASSIFY_FILTER").ok();
                    let limit = std::env::var("RZ_TRACE_CLASSIFY_LIMIT")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(50);
                    let mut count = TRACE_COUNT.get_or_init(|| Mutex::new(0)).lock().unwrap();
                    if *count < limit && filter.as_ref().map_or(true, |f| def_path.contains(f)) {
                        *count += 1;
                        rz_pass_warn!(
                            self,
                            "[rusteze][trace] classify_call_effect: {} => {:?} (filter={})",
                            def_path,
                            effect,
                            filter.as_deref().unwrap_or("<none>")
                        );
                    }
                }
                if trace_enabled && !known {
                    static TRACE_UNKNOWN_COUNT: OnceLock<Mutex<usize>> = OnceLock::new();
                    let limit = std::env::var("RZ_TRACE_CLASSIFY_UNKNOWN_LIMIT")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(20);
                    let mut count = TRACE_UNKNOWN_COUNT
                        .get_or_init(|| Mutex::new(0))
                        .lock()
                        .unwrap();
                    if *count < limit {
                        *count += 1;
                        let contains_slice_impl = def_path.contains("::slice::<impl [");
                        let ends_get = def_path.ends_with("::get");
                        let ends_get_mut = def_path.ends_with("::get_mut");
                        let ends_is_empty = def_path.ends_with("::is_empty");
                        rz_pass_warn!(
                            self,
                            "[rusteze][trace] unknown_call def_path={:?} contains_slice_impl={} ends_get={} ends_get_mut={} ends_is_empty={}",
                            def_path,
                            contains_slice_impl,
                            ends_get,
                            ends_get_mut,
                            ends_is_empty
                        );
                    }
                }
                if !known {
                    self.warn_unknown_call_once(def_path);
                }
            }
        }
    }

    pub(in crate::instrumentation) fn push_memop_call_effects<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        is_copy: bool,
        is_memset: bool,
        classified_write_ptr_local: &mut Option<Local>,
        classified_read_ptr_local: &mut Option<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
    ) {
        if !is_copy && !is_memset {
            return;
        }

        if is_copy {
            // Signature convention we assume (matches core::intrinsics and ptr wrappers):
            //   copy::<T>(src: *const T, dst: *mut T, count: usize)
            //   copy_nonoverlapping::<T>(src: *const T, dst: *mut T, count: usize)
            if args.len() >= 3 {
                let src_place = args.get(0).and_then(|a| self.place_from_operand(&a.node));
                let dst_place = args.get(1).and_then(|a| self.place_from_operand(&a.node));
                let src_local = src_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let dst_local = dst_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let count_op = &args[2].node;

                let size_op_for = |ptr_local: Local| -> SizeOperand<'tcx> {
                    self.memop_access_size_bytes(
                        tcx,
                        body,
                        ptr_local,
                        count_op,
                        term.source_info.span,
                    )
                };

                // These hooks are inserted via terminator splitting. Because later insert points
                // execute earlier, push destination WRITE before source READ so execution is:
                //   1. READ src
                //   2. WRITE dst
                //   3. perform the actual memcopy call
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size_op = size_op_for(dst);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: dst_place.unwrap_or(Place::from(dst)),
                        kind: InstrKind::PtrWrite {
                            ptr_local: dst,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                }
                if let Some(src) = src_local {
                    *classified_read_ptr_local = Some(src);
                    ptr_locals_needing_tag.insert(src);
                    let size_op = size_op_for(src);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: src_place.unwrap_or(Place::from(src)),
                        kind: InstrKind::PtrRead {
                            ptr_local: src,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                }
                if let (Some(src_place), Some(dst_place)) = (src_place, dst_place) {
                    let size_op = if let Some(src) = src_local {
                        size_op_for(src)
                    } else if let Some(dst) = dst_local {
                        size_op_for(dst)
                    } else {
                        SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0))
                    };
                    let shadow_bb = match &term.kind {
                        TerminatorKind::Call {
                            target: Some(tgt_bb),
                            ..
                        } => *tgt_bb,
                        _ => bb,
                    };
                    let shadow_stmt_idx = if shadow_bb == bb {
                        block_data.statements.len()
                    } else {
                        0
                    };
                    insert_points.push(InsertPoint {
                        bb: shadow_bb,
                        stmt_idx: shadow_stmt_idx,
                        insert_before: shadow_bb != bb,
                        source_info: term.source_info,
                        place: dst_place,
                        kind: InstrKind::ShadowCopyRange { src_place, size_op },
                    });
                }
            }
        } else if is_memset {
            // Signature convention:
            //   write_bytes::<T>(dst: *mut T, val: u8, count: usize)
            if args.len() >= 3 {
                let dst_place = args.get(0).and_then(|a| self.place_from_operand(&a.node));
                let dst_local = dst_place
                    .and_then(|p| self.resolve_ptr_local_for_call_place(tcx, body, block_data, p));
                let count_op = &args[2].node;
                if let Some(dst) = dst_local {
                    *classified_write_ptr_local = Some(dst);
                    ptr_locals_needing_tag.insert(dst);
                    let size_op = self.memop_access_size_bytes(
                        tcx,
                        body,
                        dst,
                        count_op,
                        term.source_info.span,
                    );
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: dst_place.unwrap_or(Place::from(dst)),
                        kind: InstrKind::PtrWrite {
                            ptr_local: dst,
                            size_op,
                            align_op: SizeOperand::Const(self.const_usize(
                                tcx,
                                term.source_info.span,
                                1,
                            )),
                        },
                    });
                    if let Some(dst_place) = dst_place {
                        let shadow_bb = match &term.kind {
                            TerminatorKind::Call {
                                target: Some(tgt_bb),
                                ..
                            } => *tgt_bb,
                            _ => bb,
                        };
                        let shadow_stmt_idx = if shadow_bb == bb {
                            block_data.statements.len()
                        } else {
                            0
                        };
                        insert_points.push(InsertPoint {
                            bb: shadow_bb,
                            stmt_idx: shadow_stmt_idx,
                            insert_before: shadow_bb != bb,
                            source_info: term.source_info,
                            place: dst_place,
                            kind: InstrKind::ShadowKill {
                                size_op: self.memop_access_size_bytes(
                                    tcx,
                                    body,
                                    dst,
                                    count_op,
                                    term.source_info.span,
                                ),
                            },
                        });
                    }
                }
            }
        }
    }

    pub(in crate::instrumentation) fn push_alloc_shim_effects<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        alloc_shim_kind: AllocShimKind,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
    ) {
        // Where to insert events that need the call's return value.
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };

        match alloc_shim_kind {
            AllocShimKind::Alloc | AllocShimKind::AllocZeroed => {
                // Record the newly allocated pointer as live.
                if let Some(dst_local) = destination.as_local() {
                    let dst_ty = body.local_decls[dst_local].ty;
                    if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                        // Many std::alloc wrappers take a `Layout` as arg0 instead of (size, align).
                        // For Layout-taking forms we currently record unknown size=0.
                        // TODO: extract Layout.size so we can do precise OOB.
                        let size_op: Operand<'tcx> = if let Some(arg0) = args.get(0) {
                            let arg0_ty = arg0.node.ty(body, tcx);
                            match arg0_ty.kind() {
                                TyKind::Adt(adt, _) => {
                                    let name = tcx.def_path_str(adt.did());
                                    if name.contains("core::alloc::Layout")
                                        || name.contains("alloc::alloc::Layout")
                                        || name.contains("std::alloc::Layout")
                                    {
                                        self.const_usize(tcx, term.source_info.span, 0)
                                    } else {
                                        arg0.node.clone()
                                    }
                                }
                                _ => arg0.node.clone(),
                            }
                        } else {
                            self.const_usize(tcx, term.source_info.span, 0)
                        };
                        let size_op = SizeOperand::Const(size_op);

                        // Insert in the target block so `dst_local` is initialized.
                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::Dealloc => {
                // Deallocation: record pointer as dead.
                // For Layout-taking forms we currently record unknown size=0.
                if let Some(first) = args.get(0) {
                    if let Some(p) = self.place_from_operand(&first.node) {
                        let ptr_local = p.local;
                        let ptr_ty = body.local_decls[ptr_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, ptr_ty) {
                            let size_op: Operand<'tcx> = if args.len() >= 2 {
                                let arg1_ty = args[1].node.ty(body, tcx);
                                if matches!(arg1_ty.kind(), TyKind::Uint(_)) {
                                    args[1].node.clone()
                                } else {
                                    self.const_usize(tcx, term.source_info.span, 0)
                                }
                            } else {
                                self.const_usize(tcx, term.source_info.span, 0)
                            };
                            let size_op = SizeOperand::Const(size_op);

                            ptr_locals_needing_tag.insert(ptr_local);

                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(ptr_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local,
                                    live: false,
                                    size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::Realloc => {
                // Record old ptr dead, new ptr live. Signature: (ptr, old_size, align, new_size) -> *mut u8
                if let Some(first) = args.get(0) {
                    if let Some(p) = self.place_from_operand(&first.node) {
                        let old_ptr_local = p.local;
                        let old_ptr_ty = body.local_decls[old_ptr_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, old_ptr_ty) {
                            let old_size_op: Operand<'tcx> = if args.len() >= 2 {
                                let arg1_ty = args[1].node.ty(body, tcx);
                                if matches!(arg1_ty.kind(), TyKind::Uint(_)) {
                                    args[1].node.clone()
                                } else {
                                    self.const_usize(tcx, term.source_info.span, 0)
                                }
                            } else {
                                self.const_usize(tcx, term.source_info.span, 0)
                            };
                            let old_size_op = SizeOperand::Const(old_size_op);

                            ptr_locals_needing_tag.insert(old_ptr_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(old_ptr_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: old_ptr_local,
                                    live: false,
                                    size_op: old_size_op,
                                },
                            });
                        }
                    }
                }

                if let Some(dst_local) = destination.as_local() {
                    let dst_ty = body.local_decls[dst_local].ty;
                    if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                        let new_size_op: Operand<'tcx> = if args.len() >= 4 {
                            args[3].node.clone()
                        } else if args.len() >= 3 {
                            args[2].node.clone()
                        } else {
                            self.const_usize(tcx, term.source_info.span, 0)
                        };
                        let new_size_op = SizeOperand::Const(new_size_op);

                        ptr_locals_needing_tag.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op: new_size_op,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::HeapAlloc {
                                    ptr_local: dst_local,
                                    live: true,
                                    size_op: new_size_op,
                                },
                            });
                        }
                    }
                }
            }
            AllocShimKind::No => {}
        }
    }

    pub(in crate::instrumentation) fn scan_call_terminator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        func: &Operand<'tcx>,
        args: &Box<[Spanned<Operand<'tcx>>]>,
        destination: &Place<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        projectionless_anchor_suppressed_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        local_slot_shadow_store_locals: &mut HashSet<Local>,
        local_ref_use_stats: &HashMap<Local, LocalRefUseStats>,
    ) {
        let callee_opt = self.direct_callee(tcx, body, block_data, func);
        let callee_id_opt = callee_opt.map(|(_did, cid)| cid);
        let callee_path_opt = callee_opt.map(|(did, _)| tcx.def_path_str(did));
        let callee_summary_opt =
            callee_opt.and_then(|(did, _)| unsafe_dataflow::summary_for_def_id(tcx, did));
        let callee_instrumented = callee_opt
            .map(|(did, _)| self.is_instrumented_callee(tcx, did))
            .unwrap_or(false);
        let fn_trait_call = callee_opt
            .map(|(did, _)| self.is_fn_trait_call(tcx, did))
            .unwrap_or(false);
        let direct_call_arg_callee_id = if callee_instrumented && !fn_trait_call {
            callee_id_opt
        } else {
            None
        };
        let dynamic_call_arg_transport = fn_trait_call
            || (callee_id_opt.is_none()
                && matches!(
                    func.ty(body, tcx).kind(),
                    TyKind::FnPtr(..)
                        | TyKind::FnDef(..)
                        | TyKind::Closure(..)
                        | TyKind::CoroutineClosure(..)
                ));

        // 6a: Remove is_plain_store/is_plain_load computation.

        // Centralized effect classification for direct calls.
        let mut call_effect_opt: Option<CallEffect> = callee_path_opt
            .as_deref()
            .map(|p| self.classify_call_effect(p));
        if callee_instrumented && matches!(call_effect_opt, None | Some(CallEffect::Unknown)) {
            if let Some(summary) = callee_summary_opt.as_ref() {
                if let Some(summary_effect) = self.classify_instrumented_call_effect_from_summary(
                    tcx,
                    body,
                    args,
                    destination,
                    summary,
                ) {
                    call_effect_opt = Some(summary_effect);
                }
            }
        }
        let unknown_call =
            !callee_instrumented && matches!(call_effect_opt, None | Some(CallEffect::Unknown));

        // Unknown-call warnings are suppressed now that we instrument all non-std crates.

        let mut classified_write_ptr_local: Option<Local> = None;
        let mut classified_read_ptr_local: Option<Local> = None;
        let mut classified_derive_ptr_local: Option<Local> = None;
        let mut local_ptr_derive_emitted = false;
        let mut load_shadow_emitted = false;
        let unknown_call_returns_ptr =
            unknown_call && self.is_pointer_ty(destination.ty(&body.local_decls, tcx).ty);
        let call_target_bb: Option<BasicBlock> = match &term.kind {
            TerminatorKind::Call { target, .. } => *target,
            _ => None,
        };
        if let Some(callee_id) = direct_call_arg_callee_id {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: *destination,
                kind: InstrKind::DirectCallScopeBegin { callee_id },
            });
            if let Some(tgt_bb) = call_target_bb {
                insert_points.push(InsertPoint {
                    bb: tgt_bb,
                    stmt_idx: 0,
                    insert_before: true,
                    source_info: term.source_info,
                    place: *destination,
                    kind: InstrKind::DirectCallScopeEnd { callee_id },
                });
            }
        }
        let indirect_call_needs_transport = dynamic_call_arg_transport
            && args.iter().any(|arg| {
                let Some(p) = self.place_from_operand(&arg.node) else {
                    return false;
                };
                let ty = p.ty(&body.local_decls, tcx).ty;
                self.is_pointer_ty(ty)
                    || self.supports_call_boundary_whole_slot_anchor_local(tcx, body, p.local)
                    || self.supports_call_boundary_exact_leaf_shadow_ty(tcx, body, ty)
            });
        if indirect_call_needs_transport {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place: *destination,
                kind: InstrKind::IndirectCallScopeBegin,
            });
            if let Some(tgt_bb) = call_target_bb {
                insert_points.push(InsertPoint {
                    bb: tgt_bb,
                    stmt_idx: 0,
                    insert_before: true,
                    source_info: term.source_info,
                    place: *destination,
                    kind: InstrKind::IndirectCallScopeEnd,
                });
            }
        }
        let mut noescape_reborrow_call_temps: HashSet<Local> = HashSet::new();
        for (arg_index, arg) in args.iter().enumerate() {
            if let Some(local) = self.noescape_reborrow_call_temp_local(
                tcx,
                body,
                block_data,
                func,
                local_ref_use_stats,
                arg_index,
                arg,
            ) {
                noescape_reborrow_call_temps.insert(local);
                local_slot_shadow_store_locals.insert(local);
            }
        }
        let call_is_black_box = callee_path_opt
            .as_deref()
            .is_some_and(|p| p.contains("black_box"));
        // Caller-side writeback retag set:
        // if a callee receives `&mut P` (where `P` is itself a pointer type), it may mutate
        // the caller's pointer local through that reference. Without a post-call retag, the
        // caller keeps using the stale pre-call tag for `P`, which can hide aliasing UB.
        //
        // Example:
        //   fn retarget(x: &mut &u32, t: &mut u32) { *x = &mut *(t as *mut _); }
        //   retarget(&mut target_alias, target);
        //   *target = 13;
        //   black_box(*target_alias); // must observe updated tag lineage.
        let mut post_call_writeback_retag_locals: HashSet<Local> = HashSet::new();

        // Centralized emission for direct-call effects.
        if let Some(effect) = call_effect_opt {
            match effect {
                CallEffect::Ignore => {
                    // No memory/pointer effect.
                }

                CallEffect::AllocShim(kind) => {
                    if self.heap_allocs_from_mir_enabled() {
                        // Old behavior: emit HeapAlloc hooks from MIR (may require Layout.size extraction).
                        self.push_alloc_shim_effects(
                            tcx,
                            body,
                            bb,
                            block_data,
                            term,
                            args,
                            destination,
                            kind,
                            insert_points,
                            ptr_locals_needing_tag,
                            tagged_ptr_locals,
                        );
                    } else {
                        // New default: rely on runtime global allocator wrapper for heap tracking.
                        // Still tag allocator-returned pointers so later READ/WRITE are not UNKNOWN_TAG.
                        let returns_ptr = matches!(
                            kind,
                            AllocShimKind::Alloc
                                | AllocShimKind::AllocZeroed
                                | AllocShimKind::Realloc
                        );

                        if returns_ptr {
                            if let Some(dst_local) = destination.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;

                                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                    ptr_locals_needing_tag.insert(dst_local);

                                    // IMPORTANT: destination local is initialized only after call returns.
                                    // Insert in call target block at stmt 0.
                                    let call_target_bb: Option<BasicBlock> = match &term.kind {
                                        TerminatorKind::Call { target, .. } => *target,
                                        _ => None,
                                    };

                                    if let Some(tgt_bb) = call_target_bb {
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                                exposed_provenance: false,
                                            },
                                        });
                                    } else {
                                        // Fallback: if no target, place at end of current block.
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::RawRoot {
                                                ptr_local: dst_local,
                                                is_mut: true,
                                                exposed_provenance: false,
                                            },
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::MemCopy | CallEffect::MemSet => {
                    // Memcpy/memset-style operations (intrinsics and std/core wrappers).
                    // These are real READ/WRITE effects even when there is no explicit `(*p)` deref in MIR.
                    let is_copy = matches!(effect, CallEffect::MemCopy);
                    let is_memset = matches!(effect, CallEffect::MemSet);
                    self.push_memop_call_effects(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        args,
                        is_copy,
                        is_memset,
                        &mut classified_write_ptr_local,
                        &mut classified_read_ptr_local,
                        insert_points,
                        ptr_locals_needing_tag,
                    );
                }

                CallEffect::Store | CallEffect::StoreUnaligned => {
                    // store wrapper/intrinsic: WRITE through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                let prefer_source_tag = p0.local != ptr_local
                                    && self.alias_exempt_for_ptr_ty(
                                        tcx,
                                        body,
                                        body.local_decls[p0.local].ty,
                                    );
                                let hook_ptr_local = if p0.projection.is_empty()
                                    && self.is_pointer_ty(body.local_decls[p0.local].ty)
                                {
                                    if p0.local != ptr_local {
                                        ptr_locals_needing_tag.insert(p0.local);
                                        ptr_locals_needing_tag.insert(ptr_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: p0,
                                            kind: InstrKind::TagProp {
                                                dst: p0.local,
                                                src: ptr_local,
                                                copy_tag: true,
                                                copy_ref_ancestor: true,
                                            },
                                        });
                                    }
                                    if prefer_source_tag {
                                        ptr_local
                                    } else {
                                        p0.local
                                    }
                                } else {
                                    ptr_local
                                };
                                classified_write_ptr_local = Some(hook_ptr_local);
                                ptr_locals_needing_tag.insert(hook_ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let (size_op, align_op) = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl)
                                    | TyKind::Ref(_, pointee_ty, _mutbl) => (
                                        self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            *pointee_ty,
                                            term.source_info.span,
                                        ),
                                        if matches!(effect, CallEffect::StoreUnaligned) {
                                            SizeOperand::Const(self.const_usize(
                                                tcx,
                                                term.source_info.span,
                                                1,
                                            ))
                                        } else {
                                            self.align_operand_for_ty(
                                                tcx,
                                                body,
                                                *pointee_ty,
                                                term.source_info.span,
                                            )
                                        },
                                    ),
                                    _ => (
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                    ),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrWrite {
                                        ptr_local: hook_ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });
                            }
                        }
                    }
                }

                CallEffect::Load | CallEffect::LoadUnaligned => {
                    // load wrapper/intrinsic: READ through arg0.
                    if let Some(first) = args.get(0) {
                        if let Some(p0) = self.place_from_operand(&first.node) {
                            if let Some(ptr_local) =
                                self.resolve_ptr_local_for_call_place(tcx, body, block_data, p0)
                            {
                                let hook_ptr_local = if p0.projection.is_empty()
                                    && self.is_pointer_ty(body.local_decls[p0.local].ty)
                                {
                                    if p0.local != ptr_local {
                                        ptr_locals_needing_tag.insert(p0.local);
                                        ptr_locals_needing_tag.insert(ptr_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: p0,
                                            kind: InstrKind::TagProp {
                                                dst: p0.local,
                                                src: ptr_local,
                                                copy_tag: true,
                                                copy_ref_ancestor: true,
                                            },
                                        });
                                    }
                                    p0.local
                                } else {
                                    ptr_local
                                };
                                classified_read_ptr_local = Some(hook_ptr_local);
                                ptr_locals_needing_tag.insert(hook_ptr_local);

                                let ty0 = p0.ty(&body.local_decls, tcx).ty;
                                let (size_op, align_op) = match ty0.kind() {
                                    TyKind::RawPtr(pointee_ty, _mutbl)
                                    | TyKind::Ref(_, pointee_ty, _mutbl) => (
                                        self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            *pointee_ty,
                                            term.source_info.span,
                                        ),
                                        if matches!(effect, CallEffect::LoadUnaligned) {
                                            SizeOperand::Const(self.const_usize(
                                                tcx,
                                                term.source_info.span,
                                                1,
                                            ))
                                        } else {
                                            self.align_operand_for_ty(
                                                tcx,
                                                body,
                                                *pointee_ty,
                                                term.source_info.span,
                                            )
                                        },
                                    ),
                                    _ => (
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                        SizeOperand::Const(self.const_usize(
                                            tcx,
                                            term.source_info.span,
                                            0,
                                        )),
                                    ),
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: p0,
                                    kind: InstrKind::PtrRead {
                                        ptr_local: hook_ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });

                                if let (Some(dst_local), Some(tgt_bb)) =
                                    (destination.as_local(), call_target_bb)
                                {
                                    let dst_ty = body.local_decls[dst_local].ty;
                                    let pointee_place_and_ty =
                                        self.pointer_pointee_place_and_ty(tcx, body, p0);
                                    let pointee_leaf_place =
                                        if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                            self.single_shadowable_leaf_place_for_pointer_pointee(
                                                tcx, body, p0,
                                            )
                                        } else {
                                            None
                                        };
                                    let loaded_ptr_ty = match ty0.kind() {
                                        TyKind::RawPtr(pointee_ty, _mutbl)
                                        | TyKind::Ref(_, pointee_ty, _mutbl) => Some(*pointee_ty),
                                        _ => None,
                                    };
                                    if let Some(load_src_place) = pointee_leaf_place {
                                        ptr_locals_needing_tag.insert(dst_local);
                                        tagged_ptr_locals.insert(dst_local);
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: load_src_place,
                                            kind: InstrKind::ShadowLoad {
                                                dst_local,
                                                require_tag: true,
                                                validate_ref: false,
                                            },
                                        });
                                        load_shadow_emitted = true;
                                    } else if !self.is_pointer_ty(dst_ty) {
                                        if let Some((pointee_place, pointee_ty)) =
                                            pointee_place_and_ty
                                        {
                                            let dst_leafs = self
                                                .shadowable_leaf_ptr_specs_from_place(
                                                    tcx,
                                                    body,
                                                    Place::from(dst_local),
                                                    dst_ty,
                                                );
                                            let src_leafs = self
                                                .shadowable_leaf_ptr_specs_from_place(
                                                    tcx,
                                                    body,
                                                    pointee_place,
                                                    pointee_ty,
                                                );
                                            if let Some(matched_leafs) = self
                                                .pair_shadowable_leaf_ptr_specs_from_arg0(
                                                    &dst_leafs, &src_leafs,
                                                )
                                            {
                                                for (dst_spec, src_spec) in matched_leafs {
                                                    let kind =
                                                        if src_spec.place.projection.is_empty()
                                                            && self.is_shadowable_ptr_ty(
                                                                tcx,
                                                                body,
                                                                dst_spec.ty,
                                                            )
                                                        {
                                                            ptr_locals_needing_tag
                                                                .insert(src_spec.place.local);
                                                            InstrKind::ShadowStore {
                                                                src_local: src_spec.place.local,
                                                            }
                                                        } else {
                                                            InstrKind::ShadowCopySlot {
                                                                src_place: src_spec.place,
                                                            }
                                                        };
                                                    insert_points.push(InsertPoint {
                                                        bb: tgt_bb,
                                                        stmt_idx: 0,
                                                        insert_before: true,
                                                        source_info: term.source_info,
                                                        place: dst_spec.place,
                                                        kind,
                                                    });
                                                }
                                            }
                                        }
                                    } else if self.is_shadowable_ptr_ty(tcx, body, dst_ty)
                                        && loaded_ptr_ty.is_some_and(|ty| {
                                            self.is_shadowable_ptr_ty(tcx, body, ty)
                                        })
                                    {
                                        ptr_locals_needing_tag.insert(dst_local);
                                        tagged_ptr_locals.insert(dst_local);
                                        let load_src_place =
                                            p0.project_deeper(&[PlaceElem::Deref], tcx);
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: load_src_place,
                                            kind: InstrKind::ShadowLoad {
                                                dst_local,
                                                require_tag: true,
                                                validate_ref: false,
                                            },
                                        });
                                        load_shadow_emitted = true;
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::CarrierCopyArg(source_arg_index) => {
                    if let (Some(dst_local), Some(tgt_bb)) =
                        (destination.as_local(), call_target_bb)
                    {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if !self.is_pointer_ty(dst_ty) {
                            if let Some(src_place) = args
                                .get(source_arg_index)
                                .and_then(|arg| self.place_from_operand(&arg.node))
                            {
                                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                    tcx,
                                    body,
                                    Place::from(dst_local),
                                    dst_ty,
                                );
                                let direct_src_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                    tcx, body, src_place, src_ty,
                                );
                                let pointee_src_leafs = self
                                    .pointer_pointee_place_and_ty(tcx, body, src_place)
                                    .map(|(pointee_place, pointee_ty)| {
                                        self.shadowable_leaf_ptr_specs_from_place(
                                            tcx,
                                            body,
                                            pointee_place,
                                            pointee_ty,
                                        )
                                    })
                                    .filter(|leafs| !leafs.is_empty());
                                if let Some(matched_leafs) =
                                    structural_transport::pair_return_leafs_from_source_arg(
                                        self,
                                        &dst_leafs,
                                        &direct_src_leafs,
                                        pointee_src_leafs.as_deref(),
                                    )
                                {
                                    for (dst_spec, src_spec) in matched_leafs {
                                        let kind = if src_spec.place.projection.is_empty()
                                            && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                        {
                                            ptr_locals_needing_tag.insert(src_spec.place.local);
                                            InstrKind::ShadowStore {
                                                src_local: src_spec.place.local,
                                            }
                                        } else {
                                            InstrKind::ShadowCopySlot {
                                                src_place: src_spec.place,
                                            }
                                        };
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
                                            place: dst_spec.place,
                                            kind,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                CallEffect::RefRetFromArg0PointeeLeafs => {
                    if let (Some(dst_local), Some(tgt_bb)) =
                        (destination.as_local(), call_target_bb)
                    {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if !self.is_pointer_ty(dst_ty) {
                            if let Some(src_place) = args
                                .get(0)
                                .and_then(|arg| self.place_from_operand(&arg.node))
                            {
                                let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                    tcx,
                                    body,
                                    Place::from(dst_local),
                                    dst_ty,
                                );
                                let dst_has_only_ref_leafs = !dst_leafs.is_empty()
                                    && dst_leafs.iter().all(|leaf_spec| {
                                        matches!(leaf_spec.ty.kind(), TyKind::Ref(..))
                                    });
                                if dst_has_only_ref_leafs {
                                    if let Some((pointee_place, pointee_ty)) =
                                        self.pointer_pointee_place_and_ty(tcx, body, src_place)
                                    {
                                        let src_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                            tcx,
                                            body,
                                            pointee_place,
                                            pointee_ty,
                                        );
                                        if let Some(matched_leafs) = self
                                            .pair_shadowable_leaf_ptr_specs_from_arg0(
                                                &dst_leafs, &src_leafs,
                                            )
                                        {
                                            for (dst_spec, src_spec) in matched_leafs {
                                                insert_points.push(InsertPoint {
                                                    bb: tgt_bb,
                                                    stmt_idx: 0,
                                                    insert_before: true,
                                                    source_info: term.source_info,
                                                    place: dst_spec.place,
                                                    kind: InstrKind::ShadowCopySlot {
                                                        src_place: src_spec.place,
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

                CallEffect::PtrDerive => {
                    // Pointer-result handling for ptr-derivation wrappers (add/sub/offset/as_ptr...).
                    //
                    // For raw-pointer wrappers, local PtrDerive is more robust than relying on
                    // return-boundary transport alone: tiny helpers like `as_mut_ptr` often return
                    // a projected pointer value, and if the callee never materializes a precise
                    // return tag, later dereferences degrade into UNKNOWN_TAG.
                    //
                    // Shared-ref wrappers are different. If an instrumented callee returns `&T`,
                    // the return-boundary transport (`RetPush`/`RetTake`) is the principled model:
                    // it preserves the boundary parent that the next call should retag from. A
                    // local PtrDerive on the caller side collapses that boundary state back onto
                    // the receiver/source tag and can produce stale shared children at the next
                    // call boundary (for example `Bytes::as_ref()` -> subslice -> `slice_ref`).
                    if !call_is_black_box {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            let prefer_return_boundary_for_ref =
                                callee_instrumented && matches!(dst_ty.kind(), TyKind::Ref(..));
                            let src_arg_index = callee_path_opt
                                .as_deref()
                                .map(|p| self.ptr_derive_source_arg_index(p))
                                .unwrap_or(0);
                            let src_arg_place = args
                                .get(src_arg_index)
                                .and_then(|arg| self.place_from_operand(&arg.node));
                            if !self.is_pointer_ty(dst_ty) {
                                if let (Some(src_place), Some(tgt_bb)) =
                                    (src_arg_place, call_target_bb)
                                {
                                    let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                    let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                                        tcx,
                                        body,
                                        Place::from(dst_local),
                                        dst_ty,
                                    );
                                    let direct_src_leafs = self
                                        .shadowable_leaf_ptr_specs_from_place(
                                            tcx, body, src_place, src_ty,
                                        );
                                    let pointee_src_leafs = self
                                        .pointer_pointee_place_and_ty(tcx, body, src_place)
                                        .map(|(pointee_place, pointee_ty)| {
                                            self.shadowable_leaf_ptr_specs_from_place(
                                                tcx,
                                                body,
                                                pointee_place,
                                                pointee_ty,
                                            )
                                        })
                                        .filter(|leafs| !leafs.is_empty());
                                    if let Some(matched_leafs) =
                                        structural_transport::pair_return_leafs_from_source_arg(
                                            self,
                                            &dst_leafs,
                                            &direct_src_leafs,
                                            pointee_src_leafs.as_deref(),
                                        )
                                    {
                                        for (dst_spec, src_spec) in matched_leafs {
                                            let kind = if src_spec.place.projection.is_empty()
                                                && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                            {
                                                ptr_locals_needing_tag.insert(src_spec.place.local);
                                                InstrKind::ShadowStore {
                                                    src_local: src_spec.place.local,
                                                }
                                            } else {
                                                InstrKind::ShadowCopySlot {
                                                    src_place: src_spec.place,
                                                }
                                            };
                                            insert_points.push(InsertPoint {
                                                bb: tgt_bb,
                                                stmt_idx: 0,
                                                insert_before: true,
                                                source_info: term.source_info,
                                                place: dst_spec.place,
                                                kind,
                                            });
                                        }
                                    }
                                }
                            }
                            // Allow wide-pointer destinations too (e.g., from_raw_parts_mut -> &mut [T]).
                            if self.is_pointer_ty(dst_ty) && !prefer_return_boundary_for_ref {
                                let mut derive_emitted_here = false;
                                let mut derived_from_recovered_boundary = false;
                                let direct_pointer_leaf_place = src_arg_place.and_then(|src_place| {
                                    if matches!(dst_ty.kind(), TyKind::RawPtr(..)) {
                                        self.single_direct_pointer_leaf_place_for_projectionless_carrier(
                                            tcx, body, src_place,
                                        )
                                    } else {
                                        None
                                    }
                                });
                                let prefer_parent_snapshot =
                                    src_arg_place.is_some_and(|src_place| {
                                        let src_place_ty = src_place.ty(&body.local_decls, tcx).ty;
                                        !src_place.projection.is_empty()
                                            || !self.is_pointer_ty(src_place_ty)
                                    });
                                if let (Some(src_leaf_place), Some(tgt_bb)) =
                                    (direct_pointer_leaf_place, call_target_bb)
                                {
                                    ptr_locals_needing_tag.insert(dst_local);
                                    tagged_ptr_locals.insert(dst_local);
                                    insert_points.push(InsertPoint {
                                        bb: tgt_bb,
                                        stmt_idx: 0,
                                        insert_before: true,
                                        source_info: term.source_info,
                                        place: src_leaf_place,
                                        kind: InstrKind::ShadowLoad {
                                            dst_local,
                                            require_tag: true,
                                            validate_ref: false,
                                        },
                                    });
                                    derive_emitted_here = true;
                                    load_shadow_emitted = true;
                                } else if prefer_parent_snapshot {
                                    if let Some(src_place) = src_arg_place {
                                        if boundary_recovered_ptr_locals.contains(&src_place.local)
                                        {
                                            derived_from_recovered_boundary = true;
                                        }
                                        ptr_locals_needing_tag.insert(dst_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx: block_data.statements.len(),
                                            insert_before: false,
                                            source_info: term.source_info,
                                            place: src_place,
                                            kind: InstrKind::ParentTagSnapshot {
                                                dst_local,
                                                src: src_place,
                                                is_raw_creation: !matches!(
                                                    dst_ty.kind(),
                                                    TyKind::Ref(..)
                                                ),
                                            },
                                        });
                                        Self::push_ptr_derive_parent_call(
                                            bb,
                                            block_data,
                                            term,
                                            dst_local,
                                            dst_ty,
                                            self.ptr_derive_call_strict_at_destination(
                                                body,
                                                callee_path_opt.as_deref(),
                                                call_target_bb,
                                                dst_local,
                                            ),
                                            insert_points,
                                            tagged_ptr_locals,
                                        );
                                        local_ptr_derive_emitted = true;
                                        derive_emitted_here = true;
                                    }
                                } else if let Some(src_local) = self.call_arg_pointer_source_local(
                                    tcx,
                                    body,
                                    block_data,
                                    args,
                                    src_arg_index,
                                ) {
                                    if boundary_recovered_ptr_locals.contains(&src_local) {
                                        derived_from_recovered_boundary = true;
                                    }
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);
                                    Self::push_ptr_derive_call(
                                        bb,
                                        block_data,
                                        term,
                                        dst_local,
                                        dst_ty,
                                        src_local,
                                        self.ptr_derive_call_strict_at_destination(
                                            body,
                                            callee_path_opt.as_deref(),
                                            call_target_bb,
                                            dst_local,
                                        ),
                                        insert_points,
                                        tagged_ptr_locals,
                                        &mut classified_derive_ptr_local,
                                    );
                                    local_ptr_derive_emitted = true;
                                    derive_emitted_here = true;
                                }
                                if derive_emitted_here && derived_from_recovered_boundary {
                                    boundary_recovered_ptr_locals.insert(dst_local);
                                }
                                if derive_emitted_here
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_ty)
                                {
                                    if let Some(tgt_bb) = call_target_bb {
                                        insert_points.push(InsertPoint {
                                            bb: tgt_bb,
                                            stmt_idx: 0,
                                            insert_before: true,
                                            source_info: term.source_info,
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

                CallEffect::ExposedProvenanceRoot => {
                    if let Some(dst_local) = destination.as_local() {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            if let Some(tgt_bb) = call_target_bb {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut: self.ptr_is_mut(dst_ty),
                                        exposed_provenance: true,
                                    },
                                });
                                if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                    insert_points.push(InsertPoint {
                                        bb: tgt_bb,
                                        stmt_idx: 0,
                                        insert_before: true,
                                        source_info: term.source_info,
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

                CallEffect::BoxIntoRaw => {
                    // Box::into_raw boundary modeling: root-tag + HeapAlloc live.
                    if !callee_instrumented {
                        if let Some(dst_local) = destination.as_local() {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                self.push_box_into_raw_call(
                                    tcx,
                                    body,
                                    bb,
                                    block_data,
                                    term,
                                    dst_local,
                                    dst_ty,
                                    insert_points,
                                );
                            }
                        }
                    }
                }

                CallEffect::BoxFromRaw => {
                    // Box::from_raw only rewraps an existing allocation, so do not emit
                    // any heap lifetime event here. Pointer argument tagging happens
                    // through the regular call argument handling below.
                }

                CallEffect::Unknown => {
                    // No special emission here.
                }
            }
        }

        if let (Some(tgt_bb), Some(dst_local)) = (call_target_bb, destination.as_local()) {
            let dst_ty = body.local_decls[dst_local].ty;
            if self.call_returns_std_fs_read_vec_payload(
                tcx,
                body,
                callee_path_opt.as_deref(),
                args,
                dst_ty,
            ) {
                self.push_external_vec_u8_owner_import(
                    tcx,
                    body,
                    tgt_bb,
                    0,
                    true,
                    term.source_info,
                    dst_local,
                    insert_points,
                );
            }
        }

        if let (Some(tgt_bb), Some(dst_local), Some(first_arg)) = (
            call_target_bb,
            destination.as_local(),
            args.get(0)
                .and_then(|arg| self.place_from_operand(&arg.node)),
        ) {
            let dst_ty = body.local_decls[dst_local].ty;
            let src_ty = first_arg.ty(&body.local_decls, tcx).ty;
            if args.len() == 1
                && callee_path_opt
                    .as_deref()
                    .is_some_and(|p| self.is_box_new_wrapper(p))
                && self.is_box_ty(tcx, dst_ty)
                && self.is_shadowable_ptr_ty(tcx, body, src_ty)
                && first_arg.projection.is_empty()
            {
                ptr_locals_needing_tag.insert(first_arg.local);
                insert_points.push(InsertPoint {
                    bb: tgt_bb,
                    stmt_idx: 0,
                    insert_before: true,
                    source_info: term.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::ShadowStoreBoxPointee {
                        box_local: dst_local,
                        src_local: first_arg.local,
                    },
                });
            }
        }

        if let (Some(tgt_bb), Some(dst_local)) = (call_target_bb, destination.as_local()) {
            let dst_ty = body.local_decls[dst_local].ty;
            if callee_path_opt
                .as_deref()
                .is_some_and(|p| self.is_box_new_wrapper(p))
                && self.is_box_ty(tcx, dst_ty)
            {
                let owner_leafs = self.shadowable_leaf_ptr_specs_from_place(
                    tcx,
                    body,
                    Place::from(dst_local),
                    dst_ty,
                );
                if owner_leafs.len() == 1 {
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: owner_leafs[0].place,
                        kind: InstrKind::ShadowStoreAllocRoot { is_mut: true },
                    });
                }
            }
        }

        // Treat any pointer argument as tag relevant.
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else {
                continue;
            };
            let ty = p.ty(&body.local_decls, tcx).ty;
            if (direct_call_arg_callee_id.is_some() || dynamic_call_arg_transport)
                && !self.is_pointer_ty(ty)
                && !p.projection.is_empty()
                && self.is_pointer_ty(body.local_decls[p.local].ty)
            {
                let exact_inplace_source =
                    p.projection.len() == 1 && matches!(p.projection[0], ProjectionElem::Deref);
                let suppress_protector = !self.tb_call_arg_protector_supported_for_ty(
                    tcx,
                    body,
                    body.local_decls[p.local].ty,
                );
                let flags = self.call_arg_push_flags(exact_inplace_source, suppress_protector);
                let kind = if let Some(callee_id) = direct_call_arg_callee_id {
                    InstrKind::CallArgPush {
                        callee_id,
                        arg_index: arg_index as u64,
                        ptr_local: p.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        // `*arg` is a value behind a pointer, not a pointer field. There may be
                        // no ptr-shadow entry for it, so export the source borrow family directly.
                        from_shadow: false,
                        flags,
                    }
                } else {
                    InstrKind::IndirectCallArgPush {
                        arg_index: arg_index as u64,
                        ptr_local: p.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        from_shadow: false,
                        flags,
                    }
                };
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind,
                });
            }
            if !self.is_pointer_ty(ty) && self.ty_contains_direct_ref_fields(tcx, ty) {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind: InstrKind::CallArgValidate { local: p.local },
                });
            }
            if !self.is_pointer_ty(ty) {
                continue;
            }
            let is_addr_exposable = self.is_addr_exposable_ptr_ty(tcx, body, ty);

            // If this arg is `&mut P` (P pointer-typed), track the pointee local for
            // post-call retagging in the caller to avoid stale tags after writeback.
            if let TyKind::Ref(_, pointee_ty, mutbl) = ty.kind() {
                if matches!(mutbl, Mutability::Mut) && self.is_pointer_ty(*pointee_ty) {
                    if let Some(pointee_local) =
                        self.backtrack_mut_ref_pointee_local(body, p.local, &block_data.statements)
                    {
                        if self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                            post_call_writeback_retag_locals.insert(pointee_local);
                        }
                    }
                }
            }

            let was_tagged = tagged_ptr_locals.contains(&p.local);

            // Ensure a tag exists before any call-boundary effects that consume it.
            if !was_tagged {
                let is_mut = match ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let is_ref = matches!(ty.kind(), TyKind::Ref(..));
                tagged_ptr_locals.insert(p.local);
                ptr_locals_needing_tag.insert(p.local);
                let derive_src = self
                    .recover_projected_pointer_rhs_source_local(
                        tcx,
                        body,
                        bb,
                        block_data.statements.len(),
                        p.local,
                    )
                    .or_else(|| {
                        self.backtrack_pointer_source_local(body, p.local, &block_data.statements)
                    })
                    .filter(|src_local| {
                        *src_local != p.local && tagged_ptr_locals.contains(src_local)
                    });
                if let Some(src_local) = derive_src {
                    ptr_locals_needing_tag.insert(src_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        // Use the full argument place (including projections), not just
                        // the carrier local. For projected pointer arguments, using only
                        // `p.local` retags the wrong address and desynchronizes tag pointee
                        // from the actual call operand pointer value.
                        place: p,
                        kind: InstrKind::PtrDerive {
                            dst: p.local,
                            src: src_local,
                            is_mut,
                            is_ref,
                            strict_validity: false,
                        },
                    });
                } else {
                    let root_kind = if is_ref {
                        // For reference-typed args, preserve ref semantics at the root.
                        // Emitting RawRoot here loses that information and can produce
                        // spurious stack wild-pointer reports when call-boundary tags
                        // are missing for optimized/indirect calls.
                        InstrKind::RetRoot {
                            dst_local: p.local,
                            is_mut,
                            is_ref: true,
                        }
                    } else {
                        InstrKind::RawRoot {
                            ptr_local: p.local,
                            is_mut,
                            exposed_provenance: false,
                        }
                    };
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        // Use the full argument place (including projections), not just
                        // the carrier local. For projected pointer arguments, using only
                        // `p.local` retags the wrong address and desynchronizes tag pointee
                        // from the actual call operand pointer value.
                        place: p,
                        kind: root_kind,
                    });
                }
            }

            // Inter-procedural: push argument tag to callee if instrumented.
            if direct_call_arg_callee_id.is_some() || dynamic_call_arg_transport {
                ptr_locals_needing_tag.insert(p.local);
                let suppress_protector =
                    !self.tb_call_arg_protector_supported_for_ty(tcx, body, ty);
                let flags = self.call_arg_push_flags(false, suppress_protector);
                let from_shadow = !p.projection.is_empty();
                if self.trace_call_boundary_scan_enabled() {
                    eprintln!(
                        "[rusteze][call-boundary-scan] caller={} callee_id={:?} callee={:?} arg={} place={:?} ty={:?} was_tagged={} from_shadow={} flags=0x{:x}",
                        tcx.def_path_str(body.source.def_id()),
                        direct_call_arg_callee_id,
                        callee_path_opt.as_deref(),
                        arg_index,
                        p,
                        ty,
                        was_tagged,
                        from_shadow,
                        flags
                    );
                }
                let kind = if let Some(callee_id) = direct_call_arg_callee_id {
                    InstrKind::CallArgPush {
                        callee_id,
                        arg_index: arg_index as u64,
                        ptr_local: p.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        from_shadow,
                        flags,
                    }
                } else {
                    InstrKind::IndirectCallArgPush {
                        arg_index: arg_index as u64,
                        ptr_local: p.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        from_shadow,
                        flags,
                    }
                };
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind,
                });
            }

            // Unknown call policy: conservatively model potential read/write through any pointer arg.
            if unknown_call
                && was_tagged
                && is_addr_exposable
                && !unknown_call_returns_ptr
                && matches!(ty.kind(), TyKind::Ref(..))
            {
                let is_mut_ref = matches!(ty.kind(), TyKind::Ref(_, _, Mutability::Mut));
                let size_op = match ty.kind() {
                    TyKind::RawPtr(pointee_ty, _) | TyKind::Ref(_, pointee_ty, _) => {
                        self.size_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
                    }
                    _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
                };
                let align_op = match ty.kind() {
                    TyKind::RawPtr(pointee_ty, _) | TyKind::Ref(_, pointee_ty, _) => {
                        self.align_operand_for_ty(tcx, body, *pointee_ty, term.source_info.span)
                    }
                    _ => SizeOperand::Const(self.const_usize(tcx, term.source_info.span, 0)),
                };
                ptr_locals_needing_tag.insert(p.local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind: InstrKind::PtrReadAllowUntagged {
                        ptr_local: p.local,
                        size_op: size_op.clone(),
                        align_op: align_op.clone(),
                    },
                });
                // Shared refs (`&T`) are read-only at the type level. Emitting unknown-call
                // write checks for them causes false positives in std/core helper paths
                // (e.g., compare/equality intrinsics over static data).
                if is_mut_ref {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind: InstrKind::PtrWriteAllowUntagged {
                            ptr_local: p.local,
                            size_op,
                            align_op,
                        },
                    });
                }
            }

            // Always record a coarse escape event for pointer arguments at call boundaries,
            // even when specific effects (read/write/derive) are also modeled.
            if self.filter_stdlib_uses_enabled() && self.span_is_stdlib(tcx, term.source_info.span)
            {
                continue;
            }
            let projected_carrier_raw_ptr_use = self.is_raw_pointer_ty(ty)
                && self.raw_creation_allows_no_provenance_transport(tcx, body, p);
            let noescape_reborrow_temp = noescape_reborrow_call_temps.contains(&p.local);
            if !projected_carrier_raw_ptr_use && !noescape_reborrow_temp {
                ptr_locals_needing_tag.insert(p.local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: p,
                    kind: InstrKind::PtrUse { ptr_local: p.local },
                });
            }
        }

        if direct_call_arg_callee_id.is_some() || dynamic_call_arg_transport {
            for (arg_index, a) in args.iter().enumerate() {
                let Some(p) = self.place_from_operand(&a.node) else {
                    continue;
                };
                let ty = p.ty(&body.local_decls, tcx).ty;
                if self.is_pointer_ty(ty) {
                    if fn_trait_call && arg_index == 0 {
                        if let TyKind::Ref(_, pointee_ty, _) = ty.kind() {
                            let pointee_place = p.project_deeper(&[PlaceElem::Deref], tcx);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: pointee_place,
                                kind: InstrKind::IndirectCallArgShadowRangePush {
                                    arg_index: arg_index as u64,
                                    size_op: self.size_operand_for_stack_local_ty(
                                        tcx,
                                        body,
                                        *pointee_ty,
                                        term.source_info.span,
                                    ),
                                },
                            });
                        }
                    }
                    // If `arg` is `&T`, also send pointer fields inside `*arg`.
                    for leaf_spec in self.ref_pointee_leaf_specs_from_place(tcx, body, p) {
                        Self::push_call_arg_leaf_insert(
                            insert_points,
                            bb,
                            block_data.statements.len(),
                            term.source_info,
                            direct_call_arg_callee_id,
                            arg_index,
                            leaf_spec,
                        );
                    }
                    continue;
                }
                if self.supports_call_boundary_whole_slot_anchor_local(tcx, body, p.local)
                    && self.is_whole_place_slot_family_source(tcx, body, p)
                {
                    let kind = if let Some(callee_id) = direct_call_arg_callee_id {
                        InstrKind::CallArgPush {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                            parent_mode: ParentSelectionMode::SlotFamily,
                            from_shadow: false,
                            flags: 0,
                        }
                    } else {
                        InstrKind::IndirectCallArgPush {
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                            parent_mode: ParentSelectionMode::SlotFamily,
                            from_shadow: false,
                            flags: 0,
                        }
                    };
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind,
                    });
                    if let Some(tgt_bb) = call_target_bb {
                        if self.is_shadowable_ptr_ty(tcx, body, ty) {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(p.local),
                                kind: InstrKind::ShadowStore { src_local: p.local },
                            });
                        }
                    }
                }
                if fn_trait_call && arg_index == 1 {
                    if let TyKind::Tuple(field_tys) = ty.kind() {
                        for (field_idx, field_ty) in field_tys.iter().enumerate() {
                            let field_ty = field_ty;
                            if !self.is_pointer_ty(field_ty) {
                                continue;
                            }
                            let field_place = p.project_deeper(
                                &[PlaceElem::Field(FieldIdx::from_usize(field_idx), field_ty)],
                                tcx,
                            );
                            let suppress_protector =
                                !self.tb_call_arg_protector_supported_for_ty(tcx, body, field_ty);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: field_place,
                                kind: InstrKind::IndirectCallArgPush {
                                    arg_index: (1 + field_idx) as u64,
                                    ptr_local: p.local,
                                    parent_mode: ParentSelectionMode::PointeeFamily,
                                    from_shadow: true,
                                    flags: self.call_arg_push_flags(false, suppress_protector),
                                },
                            });
                        }
                    }
                }
                if let Some(callee_id) = direct_call_arg_callee_id {
                    if self.supports_call_boundary_shadow_range_push_ty(tcx, body, ty) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: p,
                            kind: InstrKind::CallArgShadowRangePush {
                                callee_id,
                                arg_index: arg_index as u64,
                                size_op: self.size_operand_for_stack_local_ty(
                                    tcx,
                                    body,
                                    ty,
                                    term.source_info.span,
                                ),
                            },
                        });
                    }
                }
                if self.supports_call_boundary_exact_leaf_shadow_ty(tcx, body, ty) {
                    // Send each inner pointer field by exact field key.
                    for leaf_spec in self.call_boundary_leaf_ptr_specs_from_place(tcx, body, p, ty)
                    {
                        Self::push_call_arg_leaf_insert(
                            insert_points,
                            bb,
                            block_data.statements.len(),
                            term.source_info,
                            direct_call_arg_callee_id,
                            arg_index,
                            leaf_spec,
                        );
                    }
                }
            }
        }

        if !callee_instrumented && callee_id_opt.is_some() {
            let mut cleanup_callees: HashSet<u64> = HashSet::new();
            // Some std/core helpers call a closure without being instrumented.
            // Push the closure captures into the closure body side channel.
            for a in args.iter() {
                let Some(p) = self.place_from_operand(&a.node) else {
                    continue;
                };
                let ty = p.ty(&body.local_decls, tcx).ty;
                let (closure_callee_id, leaf_specs) =
                    if let Some(closure_callee_id) = self.closure_like_callee_id_for_ty(tcx, ty) {
                        if !self.supports_call_boundary_exact_leaf_shadow_ty(tcx, body, ty) {
                            continue;
                        }
                        (
                            closure_callee_id,
                            self.call_boundary_leaf_ptr_specs_from_place(tcx, body, p, ty),
                        )
                    } else if let TyKind::Ref(_, pointee_ty, _) = ty.kind() {
                        let Some(closure_callee_id) =
                            self.closure_like_callee_id_for_ty(tcx, *pointee_ty)
                        else {
                            continue;
                        };
                        (
                            closure_callee_id,
                            self.ref_pointee_leaf_specs_from_place(tcx, body, p),
                        )
                    } else {
                        continue;
                    };
                if leaf_specs.is_empty() {
                    continue;
                }
                for leaf_spec in leaf_specs {
                    Self::push_direct_call_arg_leaf_insert(
                        insert_points,
                        bb,
                        block_data.statements.len(),
                        term.source_info,
                        closure_callee_id,
                        0,
                        leaf_spec,
                    );
                    cleanup_callees.insert(closure_callee_id);
                }
            }
            if let Some(tgt_bb) = call_target_bb {
                for callee_id in cleanup_callees {
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: *destination,
                        kind: InstrKind::CallArgLeafClear { callee_id },
                    });
                }
            }
        }

        if let Some(tgt_bb) = call_target_bb {
            if let Some(dst_local) = destination.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) && interesting_stack_locals.contains(&dst_local) {
                    let mut recovered_src: Option<Local> = None;
                    let mut ambiguous = false;
                    for arg_index in 0..args.len() {
                        if let Some(src_local) = self
                            .call_arg_lineage_source_local(tcx, body, block_data, args, arg_index)
                        {
                            match recovered_src {
                                Some(existing) if existing != src_local => {
                                    ambiguous = true;
                                    break;
                                }
                                Some(_) => {}
                                None => recovered_src = Some(src_local),
                            }
                        }
                    }
                    if !ambiguous {
                        if let Some(src_local) = recovered_src {
                            let dst_ty = body.local_decls[dst_local].ty;
                            if !self.ty_contains_direct_pointer_fields(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ReborrowAnchorSeed {
                                        dst_local,
                                        src_local,
                                        mark_slot_family: false,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Caller-side writeback retag:
        // At call return, re-seed tags for pointer locals that may have been rewritten through
        // `&mut` pointer arguments. This keeps subsequent accesses tied to the updated pointer
        // value instead of the stale pre-call tag.
        if let Some(tgt_bb) = call_target_bb {
            let mut writeback_locals: Vec<Local> =
                post_call_writeback_retag_locals.into_iter().collect();
            writeback_locals.sort_by_key(|l| l.index());

            for dst_local in writeback_locals {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    continue;
                }
                ptr_locals_needing_tag.insert(dst_local);
                tagged_ptr_locals.insert(dst_local);
                if callee_instrumented && self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::ShadowLoad {
                            dst_local,
                            require_tag: true,
                            validate_ref: false,
                        },
                    });
                } else {
                    let is_mut = match dst_ty.kind() {
                        TyKind::Ref(_, _, mutbl) => matches!(mutbl, Mutability::Mut),
                        TyKind::RawPtr(_, mutbl) => matches!(mutbl, Mutability::Mut),
                        _ => false,
                    };
                    let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                    insert_points.push(InsertPoint {
                        bb: tgt_bb,
                        stmt_idx: 0,
                        insert_before: true,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::RetRoot {
                            dst_local,
                            is_mut,
                            is_ref,
                        },
                    });
                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                        insert_points.push(InsertPoint {
                            bb: tgt_bb,
                            stmt_idx: 0,
                            insert_before: true,
                            source_info: term.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::ShadowStore {
                                src_local: dst_local,
                            },
                        });
                    }
                }
            }
        }

        if callee_instrumented {
            if let Some(callee_id) = callee_id_opt {
                for (arg_index, a) in args.iter().enumerate() {
                    let Some(p) = self.place_from_operand(&a.node) else {
                        continue;
                    };
                    let ty = p.ty(&body.local_decls, tcx).ty;
                    let TyKind::Ref(_, pointee_ty, mutbl) = ty.kind() else {
                        continue;
                    };
                    if !matches!(mutbl, Mutability::Mut) {
                        continue;
                    }
                    if self.is_pointer_ty(*pointee_ty) {
                        continue;
                    }
                    if let Some(pointee_local) =
                        self.backtrack_mut_ref_pointee_local(body, p.local, &block_data.statements)
                    {
                        if self.supports_call_boundary_anchor_local(tcx, body, pointee_local) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(pointee_local),
                                kind: InstrKind::MutArgRetTake {
                                    callee_id,
                                    arg_index: arg_index as u64,
                                    local: pointee_local,
                                    ptr_local: p.local,
                                },
                            });
                            let pointee_ty = body.local_decls[pointee_local].ty;
                            for dst_leaf_spec in self.call_boundary_leaf_ptr_specs_from_place(
                                tcx,
                                body,
                                Place::from(pointee_local),
                                pointee_ty,
                            ) {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: dst_leaf_spec.place,
                                    kind: InstrKind::MutArgRetLeafTake {
                                        callee_id,
                                        arg_index: arg_index as u64,
                                        local: pointee_local,
                                        leaf_key: dst_leaf_spec.call_boundary_key(),
                                    },
                                });
                            }
                            boundary_recovered_ptr_locals.insert(p.local);
                            boundary_recovered_ptr_locals.insert(pointee_local);
                            continue;
                        }
                    }

                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(p.local),
                        kind: InstrKind::MutArgRetTakePtrOnly {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                        },
                    });
                    boundary_recovered_ptr_locals.insert(p.local);
                }
            }
        }

        // Caller-side return recovery.
        //
        // Plain pointer returns use `RetPush` / `RetTake` (or `RetRoot` for opaque callees).
        // Non-pointer return carriers use two separate channels:
        // exact pointer leaves always travel with `RetLeafPush` / `RetLeafTake`, while an outer
        // `RetAnchorPush` / `RetAnchorTake` is kept only for owner/container returns that also
        // need a whole-slot borrow family for later borrows of the returned value.
        //
        // Keep the carrier cases outside the pointer-return classifier: wrapper returns such as
        // `Result<(), BytesMut>` must still import or seed an anchor even though
        // `supports_call_boundary_ret_tag_ty` is false for the aggregate destination.
        //
        // For simple pointer-derivation wrappers like `as_mut_ptr` or `ptr.add`, we prefer the
        // local PtrDerive model over call-boundary return-tag recovery. This keeps provenance
        // stable even when the callee is instrumented but returns a projected pointer value.
        if let Some(dst_local) = destination.as_local().filter(|_| !call_is_black_box) {
            let dst_ty = body.local_decls[dst_local].ty;
            if !self.is_pointer_ty(dst_ty)
                && callee_instrumented
                && self.supports_call_boundary_shadow_range_return_ty(tcx, body, dst_ty)
            {
                if let Some(callee_id) = callee_id_opt {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::RetShadowRangeTake {
                            callee_id,
                            size_op: self.size_operand_for_stack_local_ty(
                                tcx,
                                body,
                                dst_ty,
                                term.source_info.span,
                            ),
                        },
                    });
                }
            }
            if self.supports_call_boundary_ret_tag_ty(tcx, body, dst_ty) {
                if matches!(call_effect_opt, Some(CallEffect::PtrDerive))
                    && local_ptr_derive_emitted
                {
                    // Already modeled by the local PtrDerive insertion above.
                } else if callee_instrumented {
                    if let Some(callee_id) = callee_id_opt {
                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::RetTake {
                                callee_id,
                                dst_local,
                            },
                        });
                        boundary_recovered_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb: tgt_bb,
                                    stmt_idx: 0,
                                    insert_before: true,
                                    source_info: term.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                            }
                        }
                    }
                } else {
                    // Uninstrumented callee: synthesize a fresh return tag unless another effect already
                    // models the return pointer (alloc shims/ptr-derive/Box::into_raw).
                    let alloc_returns_ptr = matches!(
                        call_effect_opt,
                        Some(CallEffect::AllocShim(
                            AllocShimKind::Alloc
                                | AllocShimKind::AllocZeroed
                                | AllocShimKind::Realloc
                        ))
                    );
                    let mut return_tagged_by_effect =
                        matches!(call_effect_opt, Some(CallEffect::BoxIntoRaw))
                            || (matches!(call_effect_opt, Some(CallEffect::PtrDerive))
                                && local_ptr_derive_emitted)
                            || matches!(call_effect_opt, Some(CallEffect::ExposedProvenanceRoot))
                            || (alloc_returns_ptr && !self.heap_allocs_from_mir_enabled())
                            || load_shadow_emitted;

                    // `core::intrinsics::read_via_copy` is classified as `Load`: when it returns
                    // a pointer value, that return is derived from arg0's pointer provenance.
                    if !return_tagged_by_effect && matches!(call_effect_opt, Some(CallEffect::Load))
                    {
                        if let Some(src_local) =
                            self.call_arg_pointer_source_local(tcx, body, block_data, args, 0)
                        {
                            ptr_locals_needing_tag.insert(dst_local);
                            ptr_locals_needing_tag.insert(src_local);
                            Self::push_ptr_derive_call(
                                bb,
                                block_data,
                                term,
                                dst_local,
                                dst_ty,
                                src_local,
                                false,
                                insert_points,
                                tagged_ptr_locals,
                                &mut classified_derive_ptr_local,
                            );
                            return_tagged_by_effect = true;
                        }
                    }

                    if !return_tagged_by_effect {
                        let is_mut = match dst_ty.kind() {
                            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            _ => false,
                        };
                        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

                        ptr_locals_needing_tag.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: true,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::RetRoot {
                                    dst_local,
                                    is_mut,
                                    is_ref,
                                },
                            });
                        }
                    }
                }
            } else if !self.is_pointer_ty(dst_ty)
                && callee_instrumented
                && self.supports_call_boundary_return_anchor_local(tcx, body, dst_local)
            {
                if let Some(callee_id) = callee_id_opt {
                    projectionless_anchor_suppressed_locals.insert(dst_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(dst_local),
                        kind: InstrKind::RetAnchorTake {
                            callee_id,
                            local: dst_local,
                        },
                    });
                    self.push_ret_leaf_take_inserts(
                        tcx,
                        body,
                        insert_points,
                        bb,
                        block_data.statements.len(),
                        term.source_info,
                        callee_id,
                        dst_local,
                        dst_ty,
                    );
                }
            } else if !self.is_pointer_ty(dst_ty)
                && callee_instrumented
                && self.supports_call_boundary_exact_leaf_shadow_ty(tcx, body, dst_ty)
            {
                if let Some(callee_id) = callee_id_opt {
                    // Leaf-only return: restore the inner pointer fields, not an outer anchor.
                    self.push_ret_leaf_take_inserts(
                        tcx,
                        body,
                        insert_points,
                        bb,
                        block_data.statements.len(),
                        term.source_info,
                        callee_id,
                        dst_local,
                        dst_ty,
                    );
                }
            } else if !self.is_pointer_ty(dst_ty)
                && self.supports_call_boundary_return_anchor_local(tcx, body, dst_local)
            {
                projectionless_anchor_suppressed_locals.insert(dst_local);
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: true,
                    source_info: term.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::RetAnchorRoot { local: dst_local },
                });
            }
        }

        let is_box_from_raw = callee_path_opt
            .as_deref()
            .is_some_and(|p| self.is_box_from_raw_wrapper(p));
        if is_box_from_raw {
            // Box::from_raw rewraps an existing allocation. Any dead HeapAlloc hook here
            // makes the destructor read look like use-after-dead (bytes::release_shared).
            insert_points.retain(|ip| {
                !(ip.bb == bb && matches!(ip.kind, InstrKind::HeapAlloc { live: false, .. }))
            });
        }

        if let Some(target_bb) = call_target_bb {
            let dst_local = destination.as_local();
            let mut kill_locals: HashSet<Local> = HashSet::new();
            for arg in args.iter() {
                let Some(arg_place) = self.place_from_operand(&arg.node) else {
                    continue;
                };
                if dst_local == Some(arg_place.local) {
                    continue;
                }
                if noescape_reborrow_call_temps.contains(&arg_place.local) {
                    kill_locals.insert(arg_place.local);
                }
            }
            let mut kill_locals: Vec<Local> = kill_locals.into_iter().collect();
            kill_locals.sort_by_key(|local| local.index());
            for ptr_local in kill_locals {
                insert_points.push(InsertPoint {
                    bb: target_bb,
                    stmt_idx: 0,
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(ptr_local),
                    kind: InstrKind::TagKill { ptr_local },
                });
            }
        }
    }
}
