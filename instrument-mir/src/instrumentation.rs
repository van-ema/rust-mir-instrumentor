use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::{Mutex, OnceLock};

// Lightweight logging macros for the compiler pass.
// These avoid repeating `if self.log_enabled(...) { eprintln!(...) }`.
macro_rules! rz_pass_log {
    ($pass:expr, $lvl:expr, $($arg:tt)*) => {{
        if ($pass).log_enabled($lvl) {
            eprintln!($($arg)*);
        }
    }};
}

macro_rules! rz_pass_warn {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Warn, $($arg)*)
    };
}

macro_rules! rz_pass_info {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Info, $($arg)*)
    };
}

macro_rules! rz_pass_trace {
    ($pass:expr, $($arg:tt)*) => {
        rz_pass_log!($pass, PassLogLevel::Trace, $($arg)*)
    };
}

mod call_effects;
mod call_scan;
mod config;
mod crate_selection;
mod debug_refs;
mod logging;
mod lowering;
mod metadata_dataflow;
mod mir_backtrack;
mod place_addr;
mod provenance_sources;
mod runtime_hooks;
mod shadow_ops;
mod ssa_anchors;
mod stack_locals;
mod statement_scan;
mod structural_transport;
mod terminator_scan;
mod type_layout;
mod types;
mod unsafe_filter;

// (rest unchanged)
// NOTE: This pass intentionally avoids instrumenting std/core/alloc directly.
use crate::unsafe_dataflow::{self, UnsafeInfluence};
pub(crate) use call_effects::debug_classify_call_effect;
pub(in crate::instrumentation) use call_effects::*;
pub(in crate::instrumentation) use call_scan::*;
use crate_selection::FunctionDefId;
pub(in crate::instrumentation) use lowering::*;
use rustc_abi::{FieldIdx, VariantIdx};
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::lang_items::LangItem;
use rustc_hir::Mutability;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret::{GlobalAlloc, Scalar};
use rustc_middle::mir::traversal;
use rustc_middle::mir::visit::{MutatingUseContext, NonUseContext, PlaceContext, Visitor};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::TyKind;
use rustc_middle::ty::{
    ConstKind as TyConstKind, GenericArgsRef, Instance, PseudoCanonicalInput, Ty, TyCtxt, TypingEnv,
};
use rustc_middle::ty::{TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor};
use rustc_span::{source_map::Spanned, Span};
pub(in crate::instrumentation) use shadow_ops::*;
pub(in crate::instrumentation) use ssa_anchors::*;
pub(in crate::instrumentation) use stack_locals::*;
pub(in crate::instrumentation) use statement_scan::*;
pub(in crate::instrumentation) use terminator_scan::*;
pub(in crate::instrumentation) use types::*;

pub(crate) struct MyOptimizationPass;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(in crate::instrumentation) enum PassLogLevel {
    Warn,
    Info,
    Trace,
}

impl MyOptimizationPass {
    fn const_u64<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: u64) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(ConstValue::Scalar(Scalar::from_u64(v)), tcx.types.u64),
        }))
    }

    fn const_usize<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: usize) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(
                ConstValue::Scalar(Scalar::from_u64(v as u64)),
                tcx.types.usize,
            ),
        }))
    }

    fn const_u8<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span, v: u8) -> Operand<'tcx> {
        Operand::Constant(Box::new(ConstOperand {
            span,
            user_ty: None,
            const_: Const::Val(ConstValue::Scalar(Scalar::from_u8(v)), tcx.types.u8),
        }))
    }

    fn call_arg_push_flags(&self, exact_inplace_source: bool, suppress_protector: bool) -> u8 {
        let mut flags = 0;
        if exact_inplace_source {
            flags |= CALL_ARG_FLAG_INPLACE_EXACT_SOURCE;
        }
        if suppress_protector {
            flags |= CALL_ARG_FLAG_NO_PROTECTOR;
        }
        flags
    }

    fn push_arg_retags_at_entry<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        local_slot_shadow_store_locals: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        projectionless_anchor_suppressed_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        entry_stmt_idx: usize,
    ) {
        let entry_bb = START_BLOCK;
        let entry_source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };
        let callee_id = self.callee_id_u64(tcx, body.source.def_id());

        // Retag pointer arguments. Wide pointers are handled by extracting the data pointer
        // during lowering, so we can safely retag them here.
        for (arg_index, arg_local) in body.args_iter().enumerate() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_pointer_ty(arg_ty) {
                ptr_locals_needing_tag.insert(arg_local);
                tagged_ptr_locals.insert(arg_local);
                insert_points.push(InsertPoint {
                    bb: entry_bb,
                    stmt_idx: entry_stmt_idx,
                    insert_before: false,
                    source_info: entry_source_info,
                    place: Place::from(arg_local),
                    kind: InstrKind::ArgRetag {
                        callee_id,
                        arg_index: arg_index as u64,
                        ptr_local: arg_local,
                    },
                });
                if self.is_shadowable_ptr_ty(tcx, body, arg_ty) {
                    if matches!(arg_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
                        local_slot_shadow_store_locals.insert(arg_local);
                    }
                    insert_points.push(InsertPoint {
                        bb: entry_bb,
                        stmt_idx: entry_stmt_idx,
                        insert_before: false,
                        source_info: entry_source_info,
                        place: Place::from(arg_local),
                        kind: InstrKind::ShadowStore {
                            src_local: arg_local,
                        },
                    });
                }
                // A reference argument can point at a struct that stores pointers.
                // Restore those inner pointer fields too.
                for leaf_spec in
                    self.ref_pointee_leaf_specs_from_place(tcx, body, Place::from(arg_local))
                {
                    insert_points.push(InsertPoint {
                        bb: entry_bb,
                        stmt_idx: entry_stmt_idx,
                        insert_before: false,
                        source_info: entry_source_info,
                        place: leaf_spec.place,
                        kind: InstrKind::ArgLeafTake {
                            callee_id,
                            arg_index: arg_index as u64,
                            leaf_key: leaf_spec.transport_key(),
                        },
                    });
                }
            } else {
                let arg_leaf_specs = self.call_boundary_leaf_ptr_specs_from_place(
                    tcx,
                    body,
                    Place::from(arg_local),
                    arg_ty,
                );
                let leaf_seeded_anchor =
                    self.supports_call_boundary_leaf_seeded_anchor_local(tcx, body, arg_local);
                if self.supports_call_boundary_whole_slot_anchor_local(tcx, body, arg_local) {
                    projectionless_anchor_suppressed_locals.insert(arg_local);
                    insert_points.push(InsertPoint {
                        bb: entry_bb,
                        stmt_idx: entry_stmt_idx,
                        insert_before: false,
                        source_info: entry_source_info,
                        place: Place::from(arg_local),
                        kind: InstrKind::ArgAnchorTake {
                            callee_id,
                            arg_index: arg_index as u64,
                            local: arg_local,
                        },
                    });
                }
                if self.supports_call_boundary_exact_leaf_shadow_local(tcx, body, arg_local) {
                    for leaf_spec in arg_leaf_specs {
                        insert_points.push(InsertPoint {
                            bb: entry_bb,
                            stmt_idx: entry_stmt_idx,
                            insert_before: false,
                            source_info: entry_source_info,
                            place: leaf_spec.place,
                            kind: InstrKind::ArgLeafTake {
                                callee_id,
                                arg_index: arg_index as u64,
                                leaf_key: leaf_spec.transport_key(),
                            },
                        });
                    }
                }
                if leaf_seeded_anchor {
                    projectionless_anchor_suppressed_locals.insert(arg_local);
                    let seed_specs = self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(arg_local),
                        arg_ty,
                    );
                    if let [seed_spec] = seed_specs.as_slice() {
                        insert_points.push(InsertPoint {
                            bb: entry_bb,
                            stmt_idx: entry_stmt_idx,
                            insert_before: false,
                            source_info: entry_source_info,
                            place: seed_spec.place,
                            kind: InstrKind::ArgAnchorSeedFromShadow { local: arg_local },
                        });
                    }
                }
            }
        }
    }

    fn scan_body<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) -> ScanResult<'tcx> {
        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut ptr_locals_needing_tag: HashSet<Local> = HashSet::new();
        let mut boundary_recovered_ptr_locals: HashSet<Local> = HashSet::new();
        let mut tagged_ptr_locals: HashSet<Local> = HashSet::new();
        let mut projectionless_anchor_suppressed_locals: HashSet<Local> = HashSet::new();
        let mut local_slot_shadow_store_locals: HashSet<Local> = HashSet::new();
        let local_ref_use_stats = self.compute_local_ref_use_stats(body);
        let ptr_locals_with_tag_sources = self.collect_ptr_locals_with_tag_sources(tcx, body);
        let summary_elidable_shared_call_ref_locals =
            self.compute_summary_elidable_shared_call_ref_locals(tcx, body);
        let mut explicitly_tracked: HashSet<Local> = HashSet::new();
        for block_data in body.basic_blocks.iter() {
            for stmt in block_data.statements.iter() {
                match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        explicitly_tracked.insert(local);
                    }
                    _ => {}
                }
            }
        }

        let mut interesting_stack_locals = self.compute_interesting_stack_locals(tcx, body);
        for arg_local in body.args_iter() {
            let arg_ty = body.local_decls[arg_local].ty;
            if !self.is_pointer_ty(arg_ty) && self.supports_slot_family_local(tcx, body, arg_local)
            {
                interesting_stack_locals.insert(arg_local);
            }
        }
        for block_data in body.basic_blocks.iter() {
            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            let TerminatorKind::Call { destination, .. } = &term.kind else {
                continue;
            };
            let Some(dst_local) = destination.as_local() else {
                continue;
            };
            let dst_ty = body.local_decls[dst_local].ty;
            if !self.is_pointer_ty(dst_ty) && self.supports_slot_family_local(tcx, body, dst_local)
            {
                interesting_stack_locals.insert(dst_local);
            }
        }
        for block_data in body.basic_blocks.iter() {
            for stmt in block_data.statements.iter() {
                let StatementKind::Assign(box (dst_place, Rvalue::Aggregate(_, _))) = &stmt.kind
                else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty)
                    && self.supports_slot_family_local(tcx, body, dst_local)
                {
                    interesting_stack_locals.insert(dst_local);
                }
            }
        }
        for local in interesting_stack_locals.iter().copied() {
            if self.supports_call_boundary_anchor_local(tcx, body, local) {
                projectionless_anchor_suppressed_locals.insert(local);
            }
        }

        // Fallback stack locals:
        // Some locals never get explicit `StorageLive/StorageDead` in optimized MIR,
        // including address-taken arguments. Example pattern:
        //   _2 = &_1;       // _1 is an argument
        //   _3 = copy (*_2);
        // Without a fallback stack alloc, the read from `_2` looks like a wild pointer.
        //
        // To handle this, we conservatively treat these locals as always live:
        //  - record a StackAlloc(live=true) at function entry
        //  - record a StackAlloc(live=false) at every return site
        //
        // This is a fallback mechanism; precise lifetime tracking via explicit
        // StorageLive/StorageDead takes precedence when available.
        let mut fallback_locals: Vec<(Local, SizeOperand<'tcx>)> = Vec::new();
        for local in body.local_decls.indices() {
            if local == RETURN_PLACE && !interesting_stack_locals.contains(&local) {
                continue;
            }
            if explicitly_tracked.contains(&local) {
                continue;
            }
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local) {
                continue;
            }
            let size_op = self.size_operand_for_stack_local_ty(tcx, body, ty, rustc_span::DUMMY_SP);
            if !self.should_emit_stack_alloc_for_size_op(&size_op) {
                continue;
            }
            fallback_locals.push((local, size_op));
        }

        let track_all_stack_allocs = self.track_all_stack_allocs_flag();

        let entry_insert_at = self.entry_insert_after_prologue(body);
        let callee_id = self.callee_id_u64(tcx, body.source.def_id());
        insert_points.push(InsertPoint {
            bb: START_BLOCK,
            stmt_idx: entry_insert_at,
            insert_before: false,
            source_info: SourceInfo {
                span: rustc_span::DUMMY_SP,
                scope: OUTERMOST_SOURCE_SCOPE,
            },
            place: Place::from(RETURN_PLACE),
            kind: InstrKind::FnEnter { callee_id },
        });
        self.push_arg_retags_at_entry(
            tcx,
            body,
            &mut insert_points,
            &mut ptr_locals_needing_tag,
            &mut local_slot_shadow_store_locals,
            &mut tagged_ptr_locals,
            &mut projectionless_anchor_suppressed_locals,
            &interesting_stack_locals,
            entry_insert_at,
        );
        for arg_local in body.args_iter() {
            if self.is_pointer_ty(body.local_decls[arg_local].ty) {
                boundary_recovered_ptr_locals.insert(arg_local);
            }
        }

        let mut return_sites: Vec<(BasicBlock, SourceInfo, usize)> = Vec::new();
        let predecessors = body.basic_blocks.predecessors();
        let ssa_anchor_entry_by_bb = self.analyze_ssa_anchor_entry_maps(
            tcx,
            body,
            &ptr_locals_with_tag_sources,
            &summary_elidable_shared_call_ref_locals,
            &interesting_stack_locals,
            track_all_stack_allocs,
        );
        let mut projected_reborrow_anchor_specs: ReborrowAnchorSpecMap = HashMap::new();

        for (bb, block_data) in traversal::preorder(body) {
            let mut byte_copy_src_for_local: HashMap<Local, Place<'tcx>> = HashMap::new();
            let mut ssa_anchor_for_expr: SsaAnchorMap =
                ssa_anchor_entry_by_bb.get(&bb).cloned().unwrap_or_default();
            let mut bb_projected_reborrow_anchor_specs: ReborrowAnchorSpecMap = HashMap::new();
            if self.log_enabled(PassLogLevel::Trace) {
                rz_pass_trace!(
                    self,
                    "[rusteze][ssa-anchor] enter bb={:?} pred_count={} anchor_count={}",
                    bb,
                    predecessors[bb].len(),
                    ssa_anchor_for_expr.len(),
                );
            }
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                self.scan_statement(
                    tcx,
                    body,
                    bb,
                    block_data,
                    stmt_idx,
                    stmt,
                    &mut insert_points,
                    &mut byte_copy_src_for_local,
                    &mut ssa_anchor_for_expr,
                    &mut bb_projected_reborrow_anchor_specs,
                    &mut ptr_locals_needing_tag,
                    &mut tagged_ptr_locals,
                    &mut boundary_recovered_ptr_locals,
                    &ptr_locals_with_tag_sources,
                    &summary_elidable_shared_call_ref_locals,
                    &interesting_stack_locals,
                    track_all_stack_allocs,
                    true,
                );
            }

            for (key, deps) in bb_projected_reborrow_anchor_specs {
                projected_reborrow_anchor_specs.entry(key).or_insert(deps);
            }

            if let Some(term) = &block_data.terminator {
                if let TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    ..
                } = &term.kind
                {
                    self.scan_call_terminator(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        func,
                        args,
                        destination,
                        &mut insert_points,
                        &mut ptr_locals_needing_tag,
                        &mut boundary_recovered_ptr_locals,
                        &mut tagged_ptr_locals,
                        &mut projectionless_anchor_suppressed_locals,
                        &interesting_stack_locals,
                        &mut local_slot_shadow_store_locals,
                        &local_ref_use_stats,
                    );
                    self.invalidate_ssa_anchors_for_call(
                        body,
                        &mut ssa_anchor_for_expr,
                        args,
                        destination,
                        true,
                    );
                }

                if let TerminatorKind::Drop { place, .. } = &term.kind {
                    self.scan_drop_terminator(
                        tcx,
                        body,
                        bb,
                        block_data,
                        term,
                        *place,
                        &mut insert_points,
                        &mut ptr_locals_needing_tag,
                        &boundary_recovered_ptr_locals,
                        &mut tagged_ptr_locals,
                        &interesting_stack_locals,
                    );
                }

                if let TerminatorKind::Return = &term.kind {
                    let callee_id = self.callee_id_u64(tcx, body.source.def_id());
                    if self.supports_call_boundary_ret_tag_ty(tcx, body, body.return_ty()) {
                        ptr_locals_needing_tag.insert(RETURN_PLACE);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(RETURN_PLACE),
                            kind: InstrKind::RetPush {
                                callee_id,
                                ptr_local: RETURN_PLACE,
                            },
                        });
                    }
                    if self.supports_call_boundary_return_anchor_local(tcx, body, RETURN_PLACE) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(RETURN_PLACE),
                            kind: InstrKind::RetAnchorPush {
                                callee_id,
                                local: RETURN_PLACE,
                            },
                        });
                    }
                    // Return exact pointer fields separately from the outer return slot.
                    // Example: `Option<&T>` returns the inner `&T` leaf.
                    let return_ty = body.return_ty();
                    if !self.is_pointer_ty(return_ty)
                        && (self.supports_call_boundary_exact_leaf_shadow_ty(tcx, body, return_ty)
                            || self.ty_contains_direct_pointer_fields(tcx, body, return_ty))
                    {
                        let leaf_ptrs = self.call_boundary_leaf_ptr_places_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            return_ty,
                        );
                        if leaf_ptrs.is_empty() {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: Place::from(RETURN_PLACE),
                                kind: InstrKind::RetValidate {
                                    callee_id,
                                    local: RETURN_PLACE,
                                },
                            });
                        }
                        for leaf_spec in self.call_boundary_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            return_ty,
                        ) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: leaf_spec.place,
                                kind: InstrKind::RetLeafPush {
                                    callee_id,
                                    leaf_key: leaf_spec.transport_key(),
                                    leaf_is_ref: matches!(leaf_spec.ty.kind(), TyKind::Ref(..)),
                                },
                            });
                        }
                    }
                    for (arg_index, arg_local) in body.args_iter().enumerate() {
                        let arg_ty = body.local_decls[arg_local].ty;
                        let TyKind::Ref(_, pointee_ty, mutbl) = arg_ty.kind() else {
                            continue;
                        };
                        if !matches!(mutbl, Mutability::Mut) || self.is_pointer_ty(*pointee_ty) {
                            continue;
                        }
                        let pointee_ty = tcx
                            .try_normalize_erasing_regions(body.typing_env(tcx), *pointee_ty)
                            .unwrap_or(*pointee_ty);
                        // Always return the live `&mut` arg tag; only sized pointees can also
                        // return exact inner pointer-field shadows.
                        let can_export_leafs = pointee_ty.is_sized(tcx, body.typing_env(tcx))
                            && self.ty_contains_pointer_fields(tcx, body, pointee_ty, 4);
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: Place::from(arg_local),
                            kind: InstrKind::MutArgRetPush {
                                callee_id,
                                arg_index: arg_index as u64,
                                ptr_local: arg_local,
                            },
                        });
                        if can_export_leafs {
                            for leaf_spec in self.call_boundary_leaf_ptr_specs_from_place(
                                tcx,
                                body,
                                Place::from(arg_local).project_deeper(&[PlaceElem::Deref], tcx),
                                pointee_ty,
                            ) {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx: block_data.statements.len(),
                                    insert_before: false,
                                    source_info: term.source_info,
                                    place: leaf_spec.place,
                                    kind: InstrKind::MutArgRetLeafPush {
                                        callee_id,
                                        arg_index: arg_index as u64,
                                        ptr_local: arg_local,
                                        leaf_key: leaf_spec.transport_key(),
                                    },
                                });
                            }
                        }
                    }
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: Place::from(RETURN_PLACE),
                        kind: InstrKind::FnExit { callee_id },
                    });
                    return_sites.push((bb, term.source_info, block_data.statements.len()));
                }
            }
        }

        // IMPORTANT ORDERING NOTE:
        // `StackAlloc` is implemented via terminator-splitting (calls in fresh blocks).
        // If we insert multiple terminator-splitting hooks at the same location, the last applied
        // hook will execute first.
        //
        // `insert_instrumentation` iterates `insert_points` in reverse, meaning:
        //   - earlier items in `insert_points` are applied later
        //   - and therefore execute earlier
        //
        // To ensure fallback entry alloc tracking runs at real function entry (after prologue) and
        // before other inserted hooks, we prepend these InsertPoints.
        let mut fallback_entry_points: Vec<InsertPoint<'tcx>> = Vec::new();
        let mut fallback_return_locals: Vec<(Local, SizeOperand<'tcx>)> = Vec::new();

        for (local, size_op) in fallback_locals.iter().cloned() {
            if local == RETURN_PLACE && !interesting_stack_locals.contains(&local) {
                continue;
            }

            // Only track stack slots that are actually address-taken (unless user forces all).
            if !track_all_stack_allocs && !interesting_stack_locals.contains(&local) {
                continue;
            }

            // Record pointer-typed locals only if their address is taken.
            let ty = body.local_decls[local].ty;
            if self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local) {
                continue;
            }

            fallback_entry_points.push(InsertPoint {
                bb: START_BLOCK,
                stmt_idx: entry_insert_at,
                // Insert after rustc's StorageLive prologue statements.
                insert_before: false,
                source_info: SourceInfo {
                    span: rustc_span::DUMMY_SP,
                    scope: OUTERMOST_SOURCE_SCOPE,
                },
                place: Place::from(local),
                kind: InstrKind::StackAlloc {
                    local,
                    live: true,
                    size_op: size_op.clone(),
                },
            });
            self.trace_stack_alloc_emit(tcx, body, local, true, &size_op, "FallbackEntry");

            if !return_sites.is_empty() {
                fallback_return_locals.push((local, size_op.clone()));
            }
        }

        // Prepend entry fallback points so they are applied last and execute first.
        insert_points.splice(0..0, fallback_entry_points);

        let mut insert_points = self.filter_insert_points_by_unsafe_dataflow(
            tcx,
            body,
            insert_points,
            unsafe_influence,
        );
        metadata_dataflow::apply_metadata_dataflow(self, body, &mut insert_points);

        ScanResult {
            insert_points,
            ptr_locals_needing_tag,
            boundary_recovered_ptr_locals,
            local_slot_shadow_store_locals,
            projected_reborrow_anchor_specs,
            projectionless_anchor_suppressed_locals,
            interesting_stack_locals,
            fallback_return_locals,
            return_sites,
        }
    }

    pub(crate) fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let trace_pass = std::env::var("RZ_TRACE_PASS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");
        let def_id = body.source.def_id();
        let def_path = tcx.def_path_str(def_id);

        if def_path.contains("runtime") {
            if trace_pass {
                println!("Skipping optimization for {}", def_path);
            }
            return;
        }

        let crate_name = tcx.crate_name(def_id.krate);

        // Skip the `runtime` crate
        if crate_name.as_str() == "runtime" {
            if trace_pass {
                println!(
                    "Skipping optimization for item in runtime crate: {:?}",
                    def_id
                );
            }
            return;
        }

        if body.coroutine.is_some() {
            // Async lowering creates coroutine state machines. Injecting locals or
            // control-flow edits in those bodies can trigger rustc recursion/cycle
            // errors (observed with Tokio). Skip coroutine bodies for now.
            if trace_pass {
                println!("Skipping optimization for coroutine body: {:?}", def_id);
            }
            return;
        }

        // Skip build scripts to avoid ICEs in codegen (e.g. wide ptr operands in build.rs).
        if crate_name.as_str() == "build_script_build" || def_path.contains("build_script_build") {
            if trace_pass {
                println!("Skipping optimization for build script: {:?}", def_id);
            }
            return;
        }

        // Skip proc-macro crates: they execute on the host during compilation
        // (derive/attribute expansion). Instrumenting them causes compile-time
        // runtime reports that are unrelated to the fuzz target itself.
        if tcx
            .sess
            .opts
            .crate_types
            .iter()
            .any(|ct| matches!(ct, rustc_session::config::CrateType::ProcMacro))
        {
            if trace_pass {
                println!("Skipping optimization for proc-macro crate: {:?}", def_id);
            }
            return;
        }

        if trace_pass {
            println!(
                "Running MyOptimizationPass on {:?} {:?}",
                body.source.def_id(),
                def_path
            );
        }

        if self.analyze_unsafe_summaries_only_enabled() {
            let unsafe_influence = unsafe_dataflow::compute_unsafe_influence(
                tcx,
                body,
                self.unsafe_dataflow_selective_enabled(),
            );
            self.log_unsafe_dataflow_call_stats(tcx, body, &unsafe_influence);
            self.log_unsafe_dataflow_summary_stats(tcx, body, &unsafe_influence);
            self.dump_unsafe_dataflow_summary(tcx, body, &unsafe_influence);
            return;
        }

        // self.print_runtime_items(tcx);

        let hooks = self.runtime_hooks(tcx);

        let unsafe_influence = unsafe_dataflow::compute_unsafe_influence(
            tcx,
            body,
            self.unsafe_dataflow_selective_enabled(),
        );
        self.log_unsafe_dataflow_summary_stats(tcx, body, &unsafe_influence);
        self.dump_unsafe_dataflow_summary(tcx, body, &unsafe_influence);
        if self.trace_unsafe_dataflow_enabled() && unsafe_influence.enabled() {
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-dflow] fn={} tainted_ptrs={} total_ptrs={}",
                def_path,
                unsafe_influence.tainted_ptr_count(),
                unsafe_influence.total_ptr_count()
            );
        }

        let scan = self.scan_body(tcx, body, &unsafe_influence);
        let reborrow_anchor_stack_locals = scan.interesting_stack_locals.clone();
        let tag_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        let export_parent_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        let export_parent_is_recovered_local_for_ptr_local =
            self.allocate_u8_locals(tcx, body, scan.ptr_locals_needing_tag.clone());
        // Per-pointer local "nearest reference ancestor" tag.
        // This is used as the preferred parent for raw/derived pointer creations so
        // provenance survives wrapper-heavy flows (casts, calls, ret-take, etc.).
        //
        // Example:
        //   let r: &u32 = &x;            // ref tag R
        //   let p1: *const u32 = r as *const u32;   // raw tag P1
        //   let p2 = unsafe { p1.add(1) };          // raw tag P2
        // For P2 we want parent lineage to stay anchored to R (nearest ref ancestor),
        // not collapse to an uninitialized/zero parent through raw-only hops.
        // Keep this map synchronized with `tag_local_for_ptr_local` initialization paths.
        let ref_ancestor_local_for_ptr_local =
            self.allocate_tag_locals(tcx, body, scan.ptr_locals_needing_tag);
        let reborrow_anchor_stack_local_keys: HashSet<Local> = reborrow_anchor_stack_locals
            .iter()
            .copied()
            .filter(|local| !self.is_pointer_ty(body.local_decls[*local].ty))
            .collect();
        let reborrow_anchor_local_for_stack_local =
            self.allocate_tag_locals(tcx, body, reborrow_anchor_stack_local_keys.clone());
        let anchor_is_slot_family_local_for_stack_local =
            self.allocate_u8_locals(tcx, body, reborrow_anchor_stack_local_keys);
        let projected_reborrow_anchor_local_for_key =
            self.allocate_reborrow_anchor_locals(tcx, body, &scan.projected_reborrow_anchor_specs);

        let mut debug_ref_bindings: HashMap<DebugRefBindingKey, DebugRefBinding> = HashMap::new();
        for (key, is_mut) in self.collect_debug_ref_bindings(tcx, body) {
            let tag_local = body.local_decls.push(LocalDecl::new(
                tcx.types.u64,
                body.source_scopes[key.scope].span,
            ));
            debug_ref_bindings.insert(
                key,
                DebugRefBinding {
                    key,
                    tag_local,
                    is_mut,
                },
            );
        }
        let mut insert_points = scan.insert_points;
        if !scan.return_sites.is_empty() {
            let mut exit_kill_tag_locals: Vec<Local> =
                tag_local_for_ptr_local.values().copied().collect();
            exit_kill_tag_locals.sort_by_key(|local| local.index());
            exit_kill_tag_locals.dedup();
            for (bb, source_info, stmt_idx) in &scan.return_sites {
                for tag_local in &exit_kill_tag_locals {
                    insert_points.push(InsertPoint {
                        bb: *bb,
                        stmt_idx: *stmt_idx,
                        insert_before: false,
                        source_info: *source_info,
                        place: Place::from(*tag_local),
                        kind: InstrKind::ExitTagLocalKill {
                            tag_local: *tag_local,
                        },
                    });
                }
            }
        }
        for binding in debug_ref_bindings.values() {
            let activation_locs =
                self.debug_ref_activation_locations(body, binding.key.scope, binding.key.raw_local);
            for (bb, stmt_idx, source_info) in activation_locs {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info,
                    place: Place::from(binding.key.raw_local),
                    kind: InstrKind::DebugRefActivate {
                        raw_local: binding.key.raw_local,
                        tag_local: binding.tag_local,
                        is_mut: binding.is_mut,
                    },
                });
            }
        }
        // Box::from_raw rewraps an existing allocation. Suppress any dead HeapAlloc
        // hook placed at its call site so drop can read the pointee safely.
        insert_points.retain(|ip| {
            if let InstrKind::HeapAlloc {
                ptr_local,
                live: false,
                ..
            } = ip.kind
            {
                if let Some(term) = body.basic_blocks[ip.bb].terminator.as_ref() {
                    if let TerminatorKind::Call {
                        args, destination, ..
                    } = &term.kind
                    {
                        let arg0_local = args
                            .get(0)
                            .and_then(|arg| self.place_from_operand(&arg.node))
                            .map(|p| p.local);
                        if arg0_local == Some(ptr_local) {
                            if let Some(dst_local) = destination.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                if let TyKind::Adt(adt, _) = dst_ty.kind() {
                                    let name = tcx.def_path_str(adt.did());
                                    if name.contains("boxed::Box") || name.contains("::boxed::Box")
                                    {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            true
        });
        self.schedule_reborrow_anchor_resets(
            body,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projected_reborrow_anchor_specs,
            &projected_reborrow_anchor_local_for_key,
            &mut insert_points,
        );
        self.schedule_reborrow_anchor_propagation(
            tcx,
            body,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.boundary_recovered_ptr_locals,
            &mut insert_points,
        );

        let mut manual_holder_managed_tag_assignments: HashSet<(BasicBlock, usize)> =
            HashSet::new();

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
            hooks,
        );
        self.insert_fallback_return_stack_allocs(
            tcx,
            body,
            &scan.fallback_return_locals,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
            hooks,
        );

        // Avoid reading uninitialized tag locals in callees: default them to 0 ("untagged").
        // IMPORTANT: do this AFTER insert_instrumentation so we don't invalidate `stmt_idx`
        // computed by scan_body for START_BLOCK (bb0).
        let mut arg_ptr_locals: HashSet<Local> = HashSet::new();
        for arg_local in body.args_iter() {
            let arg_ty = body.local_decls[arg_local].ty;
            if self.is_pointer_ty(arg_ty) {
                arg_ptr_locals.insert(arg_local);
            }
        }

        self.init_tag_locals_to_zero(tcx, body, &tag_local_for_ptr_local, &arg_ptr_locals);
        self.init_tag_locals_to_zero(
            tcx,
            body,
            &export_parent_local_for_ptr_local,
            &arg_ptr_locals,
        );
        self.init_tag_locals_to_zero(
            tcx,
            body,
            &ref_ancestor_local_for_ptr_local,
            &arg_ptr_locals,
        );
        self.init_extra_u8_locals_to_zero(
            tcx,
            body,
            &export_parent_is_recovered_local_for_ptr_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &reborrow_anchor_local_for_stack_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_u8_locals_to_zero(
            tcx,
            body,
            &anchor_is_slot_family_local_for_stack_local
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &projected_reborrow_anchor_local_for_key
                .values()
                .copied()
                .collect::<Vec<_>>(),
        );
        self.init_extra_tag_locals_to_zero(
            tcx,
            body,
            &debug_ref_bindings
                .values()
                .map(|binding| binding.tag_local)
                .collect::<Vec<_>>(),
        );

        let mut holder_points: Vec<InsertPoint<'tcx>> = Vec::new();
        self.schedule_tag_local_holder_updates(
            body,
            &tag_local_for_ptr_local,
            &manual_holder_managed_tag_assignments,
            &mut holder_points,
        );
        self.insert_instrumentation(
            tcx,
            body,
            holder_points,
            &mut manual_holder_managed_tag_assignments,
            &tag_local_for_ptr_local,
            &scan.local_slot_shadow_store_locals,
            &export_parent_local_for_ptr_local,
            &export_parent_is_recovered_local_for_ptr_local,
            &ref_ancestor_local_for_ptr_local,
            &reborrow_anchor_local_for_stack_local,
            &anchor_is_slot_family_local_for_stack_local,
            &scan.projectionless_anchor_suppressed_locals,
            &projected_reborrow_anchor_local_for_key,
            &debug_ref_bindings,
            hooks,
        );

        // Defensive fixup: instrumentation should always leave valid terminators, but avoid
        // crashing rustc if a block ends up missing one in complex crates.
        let mut missing_terminators: Vec<BasicBlock> = Vec::new();
        let body_span = body.span;
        for (bb, bd) in body.basic_blocks_mut().iter_enumerated_mut() {
            if bd.terminator.is_none() {
                missing_terminators.push(bb);
                bd.terminator = Some(Terminator {
                    source_info: SourceInfo {
                        span: body_span,
                        scope: OUTERMOST_SOURCE_SCOPE,
                    },
                    kind: TerminatorKind::Unreachable,
                });
            }
        }
        if !missing_terminators.is_empty() {
            let mut pred_map: HashMap<BasicBlock, Vec<BasicBlock>> = HashMap::new();
            for (pred_bb, pred_bd) in body.basic_blocks.iter_enumerated() {
                if let Some(pred_term) = pred_bd.terminator.as_ref() {
                    for succ_bb in pred_term.kind.successors() {
                        pred_map.entry(succ_bb).or_default().push(pred_bb);
                    }
                }
            }
            rz_pass_warn!(
                self,
                "[rusteze][warn] inserted {} Unreachable terminators to repair malformed MIR in {}",
                missing_terminators.len(),
                def_path
            );
            for bb in missing_terminators.iter().take(16) {
                let preds = pred_map.get(bb).cloned().unwrap_or_default();
                let stmt_preview = body.basic_blocks[*bb]
                    .statements
                    .iter()
                    .take(3)
                    .map(|s| format!("{:?}", s.kind))
                    .collect::<Vec<_>>()
                    .join(" | ");
                rz_pass_warn!(
                    self,
                    "[rusteze][warn] malformed bb{}: cleanup={} pred_count={} preds={:?} stmt_count={} stmt_preview={}",
                    bb.index(),
                    body.basic_blocks[*bb].is_cleanup,
                    preds.len(),
                    preds.iter().map(|p| p.index()).collect::<Vec<_>>(),
                    body.basic_blocks[*bb].statements.len(),
                    stmt_preview
                );
            }
            if missing_terminators.len() > 16 {
                rz_pass_warn!(
                    self,
                    "[rusteze][warn] malformed MIR diagnostics truncated: {} additional blocks",
                    missing_terminators.len() - 16
                );
            }
        }
    }
}
