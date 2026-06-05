//! Finds runtime hook functions and builds operands that call them.

use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn func_operand_for<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        hooks: Hooks,
        kind: &InstrKind<'tcx>,
        sp: Span,
    ) -> Operand<'tcx> {
        let def_id = match kind {
            InstrKind::Ref { .. } => hooks.def_id_ref,
            InstrKind::DebugRefActivate { .. } => hooks.def_id_debug_ref,
            InstrKind::Raw { .. } => hooks.def_id_raw,
            InstrKind::RawRoot { .. } => hooks.def_id_raw,
            InstrKind::RetRoot { is_ref, .. } => {
                if *is_ref {
                    hooks.def_id_ref
                } else {
                    hooks.def_id_raw
                }
            }
            InstrKind::StackAlloc { .. } => hooks.def_id_alloc,
            InstrKind::HeapAlloc { .. } => hooks.def_id_alloc,
            // ConstAlloc is recorded via the same allocation hook.
            InstrKind::ConstAlloc { .. } | InstrKind::ConstAllocConst { .. } => hooks.def_id_alloc,
            InstrKind::PtrWrite { .. } => hooks.def_id_write,
            InstrKind::PtrWriteAllowUntagged { .. } => hooks.def_id_write_allow_untagged,
            InstrKind::StackSlotWriteAllowUntagged { .. } => {
                hooks.def_id_local_write_allow_untagged
            }
            InstrKind::PtrRead { .. } => hooks.def_id_read,
            InstrKind::PtrReadAllowUntagged { .. } => hooks.def_id_read_allow_untagged,
            InstrKind::PtrUse { .. } => hooks.def_id_use,
            InstrKind::ShadowLoad { .. } => hooks.def_id_shadow_load_tag,
            InstrKind::ShadowStore { .. } => hooks.def_id_shadow_store_ptr,
            InstrKind::ShadowStoreBoxPointee { .. } => hooks.def_id_shadow_store_ptr,
            InstrKind::ShadowCopySlot { .. } => hooks.def_id_shadow_copy_slot,
            InstrKind::ShadowCopyRange { .. } => hooks.def_id_shadow_copy_range,
            InstrKind::ShadowKill { .. } => hooks.def_id_shadow_kill_range,
            InstrKind::TagKill { .. } => hooks.def_id_tag_kill,
            InstrKind::TagLocalKill { .. } => hooks.def_id_tag_kill,
            InstrKind::TagRetain { .. } => hooks.def_id_tag_retain,
            InstrKind::TagProp { .. } | InstrKind::TagPropFromRefAncestor { .. } => {
                hooks.def_id_use // should never become a call (handled as a plain Assign)
            }
            InstrKind::ReborrowAnchorZero { .. }
            | InstrKind::ReborrowAnchorSet { .. }
            | InstrKind::ReborrowAnchorSeed { .. }
            | InstrKind::ParentTagSnapshot { .. } => {
                hooks.def_id_use // should never become a call (handled as a plain Assign)
            }
            InstrKind::PtrDerive { is_ref, .. } | InstrKind::PtrDeriveParent { is_ref, .. } => {
                if *is_ref {
                    hooks.def_id_ref
                } else {
                    hooks.def_id_raw
                }
            }
            InstrKind::CallArgPush { .. } => hooks.def_id_push_call_arg_tag,
            InstrKind::IndirectCallScopeBegin => hooks.def_id_begin_indirect_call_arg_scope,
            InstrKind::IndirectCallScopeEnd => hooks.def_id_end_indirect_call_arg_scope,
            InstrKind::IndirectCallArgPush { .. } => hooks.def_id_push_indirect_call_arg_tag,
            InstrKind::IndirectCallArgLeafPush { .. } => {
                hooks.def_id_push_indirect_call_arg_leaf_shadow
            }
            InstrKind::CallArgValidate { .. } => hooks.def_id_validate_call_arg_tag,
            InstrKind::CallArgLeafPush { .. } => hooks.def_id_push_call_arg_leaf_shadow,
            InstrKind::CallArgLeafClear { .. } => hooks.def_id_clear_call_arg_leaf_shadows,
            InstrKind::ArgRetag { .. } | InstrKind::ArgAnchorTake { .. } => {
                hooks.def_id_take_call_arg_tag
            }
            InstrKind::ArgAnchorSeedFromShadow { .. } => hooks.def_id_shadow_load_tag,
            InstrKind::ArgLeafTake { .. } => hooks.def_id_take_call_arg_leaf_shadow,
            InstrKind::RetValidate { .. } => hooks.def_id_validate_ret_tag,
            InstrKind::RetLeafPush { .. } => hooks.def_id_push_ret_leaf_shadow,
            InstrKind::RetAnchorPush { .. } => hooks.def_id_push_ret_tag,
            InstrKind::RetPush { .. } => hooks.def_id_push_ret_tag,
            InstrKind::RetAnchorTake { .. } => hooks.def_id_take_ret_tag,
            InstrKind::RetLeafTake { .. } => hooks.def_id_take_ret_leaf_shadow,
            InstrKind::RetAnchorRoot { .. } => hooks.def_id_raw,
            InstrKind::RetTake { .. } => hooks.def_id_take_ret_tag_or_root,
            InstrKind::MutArgRetPush { .. } => hooks.def_id_push_mut_arg_ret_tag,
            InstrKind::MutArgRetLeafPush { .. } => hooks.def_id_push_mut_arg_ret_leaf_shadow,
            InstrKind::MutArgRetTake { .. } => hooks.def_id_take_mut_arg_ret_tag,
            InstrKind::MutArgRetTakePtrOnly { .. } => hooks.def_id_take_mut_arg_ret_tag_or_zero,
            InstrKind::MutArgRetLeafTake { .. } => hooks.def_id_take_mut_arg_ret_leaf_shadow,
            InstrKind::FnEnter { .. } => hooks.def_id_enter_fn,
            InstrKind::FnExit { .. } => hooks.def_id_exit_fn,
        };
        Operand::function_handle(tcx, def_id, std::iter::empty(), sp)
    }

    pub(in crate::instrumentation) fn find_def_id_by_name<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        target_name: &str,
    ) -> Option<DefId> {
        let debug = std::env::var("RZ_DEBUG_SYMBOL_LOOKUP")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                if debug {
                    println!("Searching for '{}' in runtime crate:", target_name);
                }
                let items = tcx.exported_non_generic_symbols(cnum);
                if debug {
                    println!("Found {} items", items.len());
                }
                for (symbol, _) in items {
                    match symbol {
                        ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                            if let Some(name) = tcx.opt_item_name(*def_id) {
                                if debug {
                                    println!(" - Checking item: {}", name);
                                }
                                if name.as_str() == target_name {
                                    if debug {
                                        println!(" - Match found for '{}'", target_name);
                                    }
                                    return Some(*def_id);
                                }
                            } else {
                                if debug {
                                    println!(" - Unnamed item: {:?}", def_id);
                                }
                            }
                        }
                        ExportedSymbol::NoDefId(symbol_name) => {
                            if debug {
                                println!(" - Symbol without DefId: {:?}", symbol_name);
                            }
                        }
                        ExportedSymbol::DropGlue(ty) => {
                            if debug {
                                println!(" - DropGlue for type: {:?}", ty);
                            }
                        }
                        ExportedSymbol::AsyncDropGlueCtorShim(ty) => {
                            if debug {
                                println!(" - AsyncDropGlueCtorShim for type: {:?}", ty);
                            }
                        }
                        ExportedSymbol::AsyncDropGlue(def_id, ty) => {
                            if debug {
                                println!(
                                    " - AsyncDropGlue for DefId: {:?}, type: {:?}",
                                    def_id, ty
                                );
                            }
                        }
                        ExportedSymbol::ThreadLocalShim(def_id) => {
                            if debug {
                                println!(" - ThreadLocalShim for DefId: {:?}", def_id);
                            }
                        }
                        _ => {
                            if debug {
                                println!(" - Unhandled ExportedSymbol variant");
                            }
                        }
                    }
                }
            }
        }
        println!("No match found for '{}'", target_name);
        None
    }

    pub(in crate::instrumentation) fn find_runtime_fn_def_id<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        target_name: &str,
        expected_inputs: usize,
    ) -> Option<DefId> {
        let debug = std::env::var("RZ_DEBUG_SYMBOL_LOOKUP")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");
        let mut fallback_name_match: Option<DefId> = None;

        for &cnum in tcx.crates(()).iter() {
            if tcx.crate_name(cnum).as_str() != "runtime" {
                continue;
            }
            let items = tcx.exported_non_generic_symbols(cnum);
            for (symbol, _) in items {
                let def_id = match symbol {
                    ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                        *def_id
                    }
                    _ => continue,
                };
                let Some(name) = tcx.opt_item_name(def_id) else {
                    continue;
                };
                if name.as_str() != target_name {
                    continue;
                }
                fallback_name_match.get_or_insert(def_id);

                let sig = tcx.fn_sig(def_id).skip_binder();
                let inputs = sig.inputs().skip_binder().len();
                if debug {
                    println!(
                        "candidate runtime hook {} => {:?} {}({} args)",
                        target_name,
                        def_id,
                        tcx.def_path_str(def_id),
                        inputs
                    );
                }
                if inputs == expected_inputs {
                    return Some(def_id);
                }
            }
        }

        fallback_name_match
    }

    pub(in crate::instrumentation) fn runtime_hooks<'tcx>(&self, tcx: TyCtxt<'tcx>) -> Hooks {
        let def_id_ref = self
            .find_runtime_fn_def_id(tcx, "__record_ref_creation", 6)
            .expect("missing '__record_ref_creation' definition");
        let def_id_ref_with_extent = self
            .find_runtime_fn_def_id(tcx, "__record_ref_creation_with_extent", 8)
            .expect("missing '__record_ref_creation_with_extent' definition");
        let def_id_debug_ref = self
            .find_runtime_fn_def_id(tcx, "__record_debug_ref_creation", 7)
            .expect("missing '__record_debug_ref_creation' definition");
        let def_id_raw = self
            .find_runtime_fn_def_id(tcx, "__record_raw_ptr_creation", 6)
            .expect("missing '__record_raw_ptr_creation' definition");
        let def_id_alloc = self
            .find_runtime_fn_def_id(tcx, "__rz_record_alloc", 3)
            .expect("missing '__rz_record_alloc' definition");
        let def_id_write = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write", 5)
            .expect("missing '__rz_ptr_write' definition");
        let def_id_write_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_write_allow_untagged", 5)
            .expect("missing '__rz_ptr_write_allow_untagged' definition");
        let def_id_local_write_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_local_write_allow_untagged", 3)
            .expect("missing '__rz_local_write_allow_untagged' definition");
        let def_id_read = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read", 5)
            .expect("missing '__rz_ptr_read' definition");
        let def_id_read_allow_untagged = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_read_allow_untagged", 5)
            .expect("missing '__rz_ptr_read_allow_untagged' definition");
        let def_id_use = self
            .find_runtime_fn_def_id(tcx, "__rz_ptr_use", 2)
            .expect("missing '__rz_ptr_use' definition");
        let def_id_push_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_call_arg_boundary_tag", 7)
            .expect("missing '__rz_push_call_arg_boundary_tag' definition");
        let def_id_begin_indirect_call_arg_scope = self
            .find_runtime_fn_def_id(tcx, "__rz_begin_indirect_call_arg_scope", 0)
            .expect("missing '__rz_begin_indirect_call_arg_scope' definition");
        let def_id_end_indirect_call_arg_scope = self
            .find_runtime_fn_def_id(tcx, "__rz_end_indirect_call_arg_scope", 0)
            .expect("missing '__rz_end_indirect_call_arg_scope' definition");
        let def_id_push_indirect_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_indirect_call_arg_boundary_tag", 6)
            .expect("missing '__rz_push_indirect_call_arg_boundary_tag' definition");
        let def_id_push_indirect_call_arg_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_indirect_call_arg_leaf_shadow", 3)
            .expect("missing '__rz_push_indirect_call_arg_leaf_shadow' definition");
        let def_id_validate_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_call_arg_tag", 1)
            .expect("missing '__rz_validate_call_arg_tag' definition");
        let def_id_take_call_arg_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_tag", 4)
            .expect("missing '__rz_take_call_arg_tag' definition");
        let def_id_take_call_arg_tag_anchor = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_tag_anchor", 3)
            .expect("missing '__rz_take_call_arg_tag_anchor' definition");
        let def_id_push_call_arg_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_call_arg_leaf_shadow", 4)
            .expect("missing '__rz_push_call_arg_leaf_shadow' definition");
        let def_id_take_call_arg_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_take_call_arg_leaf_shadow", 4)
            .expect("missing '__rz_take_call_arg_leaf_shadow' definition");
        let def_id_clear_call_arg_leaf_shadows = self
            .find_runtime_fn_def_id(tcx, "__rz_clear_call_arg_leaf_shadows", 1)
            .expect("missing '__rz_clear_call_arg_leaf_shadows' definition");
        let def_id_push_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_ret_tag", 3)
            .expect("missing '__rz_push_ret_tag' definition");
        let def_id_validate_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_ret_tag", 2)
            .expect("missing '__rz_validate_ret_tag' definition");
        let def_id_take_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_tag", 2)
            .expect("missing '__rz_take_ret_tag' definition");
        let def_id_push_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_ret_leaf_shadow", 4)
            .expect("missing '__rz_push_ret_leaf_shadow' definition");
        let def_id_take_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_leaf_shadow", 3)
            .expect("missing '__rz_take_ret_leaf_shadow' definition");
        let def_id_validate_loaded_ref_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_validate_loaded_ref_tag", 1)
            .expect("missing '__rz_validate_loaded_ref_tag' definition");
        let def_id_require_loaded_ptr_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_require_loaded_ptr_tag", 1)
            .expect("missing '__rz_require_loaded_ptr_tag' definition");
        let def_id_take_ret_tag_or_root = self
            .find_runtime_fn_def_id(tcx, "__rz_take_ret_tag_or_root", 6)
            .expect("missing '__rz_take_ret_tag_or_root' definition");
        let def_id_push_mut_arg_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_push_mut_arg_ret_tag", 4)
            .expect("missing '__rz_push_mut_arg_ret_tag' definition");
        let def_id_take_mut_arg_ret_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_tag", 3)
            .expect("missing '__rz_take_mut_arg_ret_tag' definition");
        let def_id_take_mut_arg_ret_tag_or_zero = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_tag_or_zero", 3)
            .expect("missing '__rz_take_mut_arg_ret_tag_or_zero' definition");
        let def_id_push_mut_arg_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_push_mut_arg_ret_leaf_shadow", 5)
            .expect("missing '__rz_push_mut_arg_ret_leaf_shadow' definition");
        let def_id_take_mut_arg_ret_leaf_shadow = self
            .find_runtime_fn_def_id(tcx, "__rz_take_mut_arg_ret_leaf_shadow", 5)
            .expect("missing '__rz_take_mut_arg_ret_leaf_shadow' definition");
        let def_id_enter_fn = self
            .find_runtime_fn_def_id(tcx, "__rz_enter_fn", 1)
            .expect("missing '__rz_enter_fn' definition");
        let def_id_exit_fn = self
            .find_runtime_fn_def_id(tcx, "__rz_exit_fn", 1)
            .expect("missing '__rz_exit_fn' definition");
        let def_id_shadow_store_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_store_ptr", 5)
            .expect("missing '__rz_shadow_store_ptr' definition");
        let def_id_shadow_store_ptr_local = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_store_ptr_local", 5)
            .expect("missing '__rz_shadow_store_ptr_local' definition");
        let def_id_shadow_load_tag = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_tag", 1)
            .expect("missing '__rz_shadow_load_tag' definition");
        let def_id_shadow_load_tag_for_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_tag_for_ptr", 2)
            .expect("missing '__rz_shadow_load_tag_for_ptr' definition");
        let def_id_shadow_load_ref_ancestor = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_ref_ancestor", 1)
            .expect("missing '__rz_shadow_load_ref_ancestor' definition");
        let def_id_shadow_load_ref_ancestor_for_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_ref_ancestor_for_ptr", 2)
            .expect("missing '__rz_shadow_load_ref_ancestor_for_ptr' definition");
        let def_id_shadow_load_export_parent = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent", 1)
            .expect("missing '__rz_shadow_load_export_parent' definition");
        let def_id_shadow_load_export_parent_for_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent_for_ptr", 2)
            .expect("missing '__rz_shadow_load_export_parent_for_ptr' definition");
        let def_id_shadow_load_export_parent_recovered = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent_recovered", 1)
            .expect("missing '__rz_shadow_load_export_parent_recovered' definition");
        let def_id_shadow_load_export_parent_recovered_for_ptr = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_load_export_parent_recovered_for_ptr", 2)
            .expect("missing '__rz_shadow_load_export_parent_recovered_for_ptr' definition");
        let def_id_shadow_kill_range = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_kill_range", 2)
            .expect("missing '__rz_shadow_kill_range' definition");
        let def_id_tag_kill = self
            .find_runtime_fn_def_id(tcx, "__rz_tag_kill", 1)
            .expect("missing '__rz_tag_kill' definition");
        let def_id_tag_retain = self
            .find_runtime_fn_def_id(tcx, "__rz_tag_retain", 1)
            .expect("missing '__rz_tag_retain' definition");
        let def_id_shadow_copy_slot = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_copy_slot", 2)
            .expect("missing '__rz_shadow_copy_slot' definition");
        let def_id_shadow_copy_range = self
            .find_runtime_fn_def_id(tcx, "__rz_shadow_copy_range", 3)
            .expect("missing '__rz_shadow_copy_range' definition");

        let hooks = Hooks {
            def_id_ref,
            def_id_ref_with_extent,
            def_id_debug_ref,
            def_id_raw,
            def_id_alloc,
            def_id_write,
            def_id_write_allow_untagged,
            def_id_local_write_allow_untagged,
            def_id_read,
            def_id_read_allow_untagged,
            def_id_use,
            def_id_push_call_arg_tag,
            def_id_begin_indirect_call_arg_scope,
            def_id_end_indirect_call_arg_scope,
            def_id_push_indirect_call_arg_tag,
            def_id_push_indirect_call_arg_leaf_shadow,
            def_id_validate_call_arg_tag,
            def_id_take_call_arg_tag,
            def_id_take_call_arg_tag_anchor,
            def_id_push_call_arg_leaf_shadow,
            def_id_take_call_arg_leaf_shadow,
            def_id_clear_call_arg_leaf_shadows,
            def_id_push_ret_tag,
            def_id_validate_ret_tag,
            def_id_take_ret_tag,
            def_id_push_ret_leaf_shadow,
            def_id_take_ret_leaf_shadow,
            def_id_validate_loaded_ref_tag,
            def_id_require_loaded_ptr_tag,
            def_id_take_ret_tag_or_root,
            def_id_push_mut_arg_ret_tag,
            def_id_take_mut_arg_ret_tag,
            def_id_take_mut_arg_ret_tag_or_zero,
            def_id_push_mut_arg_ret_leaf_shadow,
            def_id_take_mut_arg_ret_leaf_shadow,
            def_id_enter_fn,
            def_id_exit_fn,
            def_id_shadow_store_ptr,
            def_id_shadow_store_ptr_local,
            def_id_shadow_load_tag,
            def_id_shadow_load_tag_for_ptr,
            def_id_shadow_load_ref_ancestor,
            def_id_shadow_load_ref_ancestor_for_ptr,
            def_id_shadow_load_export_parent,
            def_id_shadow_load_export_parent_for_ptr,
            def_id_shadow_load_export_parent_recovered,
            def_id_shadow_load_export_parent_recovered_for_ptr,
            def_id_shadow_kill_range,
            def_id_tag_kill,
            def_id_tag_retain,
            def_id_shadow_copy_slot,
            def_id_shadow_copy_range,
        };
        hooks
    }
}
