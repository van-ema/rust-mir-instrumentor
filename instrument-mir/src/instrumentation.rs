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
mod config;
mod crate_selection;
mod debug_refs;
mod logging;
mod metadata_dataflow;
mod mir_backtrack;
mod place_addr;
mod provenance_sources;
mod runtime_hooks;
mod ssa_anchors;
mod stack_locals;
mod structural_transport;
mod type_layout;
mod types;
mod unsafe_filter;

// (rest unchanged)
// NOTE: This pass intentionally avoids instrumenting std/core/alloc directly.
use crate::unsafe_dataflow::{self, UnsafeInfluence};
pub(crate) use call_effects::debug_classify_call_effect;
pub(in crate::instrumentation) use call_effects::*;
use crate_selection::FunctionDefId;
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
pub(in crate::instrumentation) use ssa_anchors::*;
pub(in crate::instrumentation) use stack_locals::*;
pub(in crate::instrumentation) use types::*;

pub(crate) struct MyOptimizationPass;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(in crate::instrumentation) enum PassLogLevel {
    Warn,
    Info,
    Trace,
}

impl MyOptimizationPass {
    /// Best-effort check: does this span come from the Rust std/core/alloc sources?
    /// This is used to suppress noisy PtrUse hooks for std wrappers (e.g. println!).
    fn span_is_stdlib<'tcx>(&self, tcx: TyCtxt<'tcx>, span: Span) -> bool {
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

    /// Ensure `ptr_local` has a tag by synthesizing a `RawRoot` before the current statement
    /// if it hasn't been tagged yet.
    ///
    /// For wide pointers (`&[T]`, `&str`, `dyn Trait`), RawRoot lowering will first extract the
    /// thin data pointer (dropping metadata) and root-tag that address.
    fn ensure_raw_root_before<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        ptr_local: Local,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
    ) {
        if tagged_ptr_locals.contains(&ptr_local)
            || ptr_locals_with_tag_sources.contains(&ptr_local)
        {
            return;
        }

        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_pointer_ty(ptr_ty) {
            return;
        }

        let is_mut = self.ptr_is_mut(ptr_ty);
        let is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
        let projected_src =
            self.recover_projected_pointer_rhs_source(tcx, body, bb, stmt_idx, ptr_local);
        tagged_ptr_locals.insert(ptr_local);
        ptr_locals_needing_tag.insert(ptr_local);
        insert_points.push(InsertPoint {
            bb: projected_src.map_or(bb, |(def_bb, _, _)| def_bb),
            stmt_idx: projected_src.map_or(stmt_idx, |(_, def_stmt_idx, _)| def_stmt_idx),
            insert_before: projected_src.is_none(),
            source_info,
            place: Place::from(ptr_local),
            kind: if let Some((_def_bb, _def_stmt_idx, src_place)) = projected_src {
                if is_ref {
                    let bk = match ptr_ty.kind() {
                        TyKind::Ref(_, _, Mutability::Mut) => BorrowKind::Mut {
                            kind: MutBorrowKind::Default,
                        },
                        _ => BorrowKind::Shared,
                    };
                    InstrKind::Ref {
                        bk,
                        src: src_place,
                        projected_reborrow_anchor_key: None,
                    }
                } else {
                    InstrKind::Raw {
                        is_mut,
                        src: src_place,
                    }
                }
            } else if is_ref {
                InstrKind::RetRoot {
                    dst_local: ptr_local,
                    is_mut,
                    is_ref: true,
                }
            } else {
                InstrKind::RawRoot {
                    ptr_local,
                    is_mut,
                    exposed_provenance: false,
                }
            },
        });
    }

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

    fn shadow_store_uses_local_slot_store<'tcx>(
        &self,
        local_slot_shadow_store_locals: &HashSet<Local>,
        place: Place<'tcx>,
        src_local: Local,
    ) -> bool {
        place.as_local() == Some(src_local)
            && place.projection.is_empty()
            && local_slot_shadow_store_locals.contains(&src_local)
    }

    /// Compute the set of stack locals worth tracking as allocations.
    ///
    /// We track locals whose address is taken (via `&` / `&raw`) so range-based allocation
    /// lookup and OOB checks work for stack data. This includes "address-of through deref"
    /// patterns that appear in optimized MIR, such as:
    ///   _r = &_x;
    ///   _p = &raw const (*_r);
    /// In that case, we must treat `_x` as address-taken (not just `_r`), otherwise no
    /// StackAlloc is emitted for the actual stack slot and the runtime will see WILD_POINTER.
    ///
    /// Pointer-typed locals are included only when *their own* address is taken (e.g., `&&T`),
    /// which avoids false OOB reports when simply reading a pointer value through `*const *const T`.
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
            } else {
                let arg_leaf_specs = self.shadowable_leaf_ptr_specs_from_place(
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
                if self.supports_call_boundary_leaf_shadow_local(tcx, body, arg_local) {
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

    fn scan_statement<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        stmt_idx: usize,
        stmt: &Statement<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        byte_copy_src_for_local: &mut HashMap<Local, Place<'tcx>>,
        ssa_anchor_for_expr: &mut SsaAnchorMap,
        projected_reborrow_anchor_specs: &mut ReborrowAnchorSpecMap,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &mut HashSet<Local>,
        ptr_locals_with_tag_sources: &HashSet<Local>,
        summary_elidable_shared_call_ref_locals: &HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
        track_all_stack_allocs: bool,
        trace_ssa_anchor: bool,
    ) {
        // Stack allocation lifetime: StorageLive/StorageDead.
        match stmt.kind {
            StatementKind::StorageDead(local) => {
                byte_copy_src_for_local.remove(&local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, local);
                let ty = body.local_decls[local].ty;
                if (self.is_shadowable_ptr_ty(tcx, body, ty)
                    || self.ty_contains_pointer_fields(tcx, body, ty, 8))
                    && !self.should_skip_storage_dead_shadow_kill(
                        tcx, body, block_data, stmt_idx, local,
                    )
                {
                    // Stack slots for pointer-carrying locals are often reused for unrelated
                    // scalars later in optimized MIR. Clear any residual ptr-shadow metadata at
                    // the lifetime boundary so a later non-pointer local cannot inherit stale
                    // reference/raw lineage from the old occupant.
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: Place::from(local),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(tcx, body, ty, stmt.source_info.span),
                        },
                    });
                }
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // NOTE: optimized MIR can place StorageDead before the last use
                    // through an outstanding reference. Emitting dead-by-default here
                    // causes false UAFs, so this remains opt-in.
                    if !self.use_storage_dead_enabled() {
                        return;
                    }
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: false,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                false,
                                &size_op,
                                "StorageDead",
                            );
                        }
                    }
                    return;
                }
            }
            StatementKind::StorageLive(local) => {
                let ty = body.local_decls[local].ty;
                if self.is_shadowable_ptr_ty(tcx, body, ty)
                    || self.ty_contains_pointer_fields(tcx, body, ty, 8)
                {
                    // This cleanup is independent from stack-allocation tracking: optimized MIR
                    // regularly reuses caller-frame stack slots for short-lived callee temps
                    // such as `NonNull<T>` carriers. Clear any residual ptr-shadow as soon as
                    // the new local becomes live so projected field copies do not inherit
                    // stale lineage from the previous occupant.
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: Place::from(local),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(tcx, body, ty, stmt.source_info.span),
                        },
                    });
                }
                if (track_all_stack_allocs || interesting_stack_locals.contains(&local))
                    && (local != RETURN_PLACE || interesting_stack_locals.contains(&local))
                {
                    // Record pointer-typed locals only if their address is taken (interesting locals).
                    if !(self.is_pointer_ty(ty) && !interesting_stack_locals.contains(&local)) {
                        let size_op = self.size_operand_for_stack_local_ty(
                            tcx,
                            body,
                            ty,
                            stmt.source_info.span,
                        );
                        if self.should_emit_stack_alloc_for_size_op(&size_op) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(local),
                                kind: InstrKind::StackAlloc {
                                    local,
                                    live: true,
                                    size_op: size_op.clone(),
                                },
                            });
                            self.trace_stack_alloc_emit(
                                tcx,
                                body,
                                local,
                                true,
                                &size_op,
                                "StorageLive",
                            );
                        }
                    }
                }
            }
            _ => {}
        }

        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                byte_copy_src_for_local.remove(&dst_local);
                self.invalidate_ssa_anchors_for_local(ssa_anchor_for_expr, dst_local);
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_pointer_ty(dst_ty)
                    && self.rhs_carries_boundary_recovered_ptr(
                        body,
                        rvalue,
                        boundary_recovered_ptr_locals,
                    )
                {
                    boundary_recovered_ptr_locals.insert(dst_local);
                }
                if self.is_one_byte_sized_ty(tcx, dst_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_one_byte_sized_ty(tcx, src_ty)
                            && self.place_contains_deref(src_place)
                        {
                            byte_copy_src_for_local.insert(dst_local, src_place);
                        }
                    }
                }
            }
        }

        // Pointer read: plain deref load in a statement, e.g. `_dst = copy (*p)` or `_dst = move (*p)`.
        // This is not a call/intrinsic, so we must classify it explicitly as a READ.
        if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
            if let Rvalue::Use(op) = rhs {
                let deref_place: Option<&Place<'tcx>> = match op {
                    Operand::Copy(p) | Operand::Move(p) => Some(p),
                    _ => None,
                };

                if let Some(p) = deref_place {
                    let is_deref_read = p
                        .projection
                        .iter()
                        .next()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if is_deref_read {
                        // `p` is a deref place `(*ptr_local) ...` so the base pointer local is `p.local`.
                        let ptr_local = p.local;
                        let ptr_ty = body.local_decls[ptr_local].ty;
                        // Skip reads through &'static references. These point to global memory
                        // that we don't track, and treating them as wild would be a false positive.
                        let skip_static_ref_read =
                            matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                        let skip_vtable_read = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                        if !skip_static_ref_read && !skip_vtable_read && self.is_pointer_ty(ptr_ty)
                        {
                            // Best-effort size: use the type of the dereferenced/read place.
                            let loaded_ty = p.ty(&body.local_decls, tcx).ty;
                            let read_ty = loaded_ty;
                            // Loading function pointers or vtable-like structs should not trigger
                            // memory access checks; treat these as benign metadata reads.
                            let skip_fn_ptr_read =
                                matches!(loaded_ty.kind(), TyKind::FnPtr(..) | TyKind::FnDef(..))
                                    || self.is_fn_table_adt_ty(tcx, loaded_ty);
                            let skip_vtable_field_read = match read_ty.kind() {
                                TyKind::FnPtr(..) | TyKind::FnDef(..) => true,
                                TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                                    self.is_fn_table_adt_ty(tcx, *pointee)
                                }
                                _ => self.is_fn_table_adt_ty(tcx, read_ty),
                            };
                            // Keep function-pointer and vtable-like metadata loads silent, but
                            // still instrument pointer-valued memory reads. We need those for
                            // unaligned ptr-to-ptr loads and similar cases where the loaded value
                            // is itself a pointer and the UB happens at the read.
                            if skip_fn_ptr_read || skip_vtable_field_read {
                                // Skip only the READ instrumentation; continue scanning this stmt.
                            } else {
                                let size_op = self.size_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
                                    loaded_ty,
                                    stmt.source_info.span,
                                );
                                let align_op = self.align_operand_for_deref(
                                    tcx,
                                    body,
                                    ptr_local,
                                    stmt.source_info.span,
                                );

                                self.ensure_raw_root_before(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    stmt.source_info,
                                    ptr_local,
                                    insert_points,
                                    ptr_locals_needing_tag,
                                    tagged_ptr_locals,
                                    ptr_locals_with_tag_sources,
                                );
                                ptr_locals_needing_tag.insert(ptr_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: true,
                                    source_info: stmt.source_info,
                                    place: p.clone(),
                                    kind: InstrKind::PtrRead {
                                        ptr_local,
                                        size_op,
                                        align_op,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Pointer write: any assignment whose LHS place begins with a Deref projection.
        if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
            let is_deref_write = lhs_place
                .projection
                .iter()
                .next()
                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
            if is_deref_write {
                let ptr_local = lhs_place.local;
                let ptr_ty = body.local_decls[ptr_local].ty;
                // Skip writes through &'static references. They are immutable by type,
                // and we don't track global memory for validity.
                let skip_static_ref_write =
                    matches!(ptr_ty.kind(), TyKind::Ref(region, ..) if region.is_static());
                let skip_vtable_write = self.is_vtable_like_ptr_ty(tcx, ptr_ty);
                if !skip_static_ref_write && !skip_vtable_write && self.is_pointer_ty(ptr_ty) {
                    // Best-effort size: use the type of the *place being written* (after projections).
                    // This yields the correct size for patterns like `(*p).field = ...` or `(*p)[i] = ...`.
                    let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                    let size_op = self.size_operand_for_deref(
                        tcx,
                        body,
                        ptr_local,
                        lhs_ty,
                        stmt.source_info.span,
                    );
                    let align_op =
                        self.align_operand_for_deref(tcx, body, ptr_local, stmt.source_info.span);

                    self.ensure_raw_root_before(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        stmt.source_info,
                        ptr_local,
                        insert_points,
                        ptr_locals_needing_tag,
                        tagged_ptr_locals,
                        ptr_locals_with_tag_sources,
                    );
                    ptr_locals_needing_tag.insert(ptr_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: true,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::PtrWrite {
                            ptr_local,
                            size_op,
                            align_op,
                        },
                    });
                }
            }
        }

        // Direct stack-slot write: assignment to a tracked local/field without an initial Deref.
        // Use the local's reborrow anchor as the access tag so writes through the root local
        // invalidate stale children once that local has been borrowed. Keep this allow-untagged:
        // most locals are never borrowed, and a zero anchor should stay silent.
        if let StatementKind::Assign(box (lhs_place, _rhs)) = &stmt.kind {
            let begins_with_deref = lhs_place
                .projection
                .iter()
                .next()
                .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
            if !begins_with_deref {
                let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;
                if interesting_stack_locals.contains(&lhs_place.local)
                    && !self.is_pointer_ty(lhs_ty)
                {
                    let size_op =
                        self.size_operand_for_ty(tcx, body, lhs_ty, stmt.source_info.span);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::StackSlotWriteAllowUntagged {
                            local: lhs_place.local,
                            size_op,
                            align_op: self.align_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
                    });
                }
            }
        }

        // Tag propagation across pointer-to-pointer casts and plain copies or moves of pointer locals.
        // Include wide pointers so tags survive unsize and reborrow patterns before a thin data
        // pointer is extracted later in MIR.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_pointer_ty(dst_ty) {
                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                        if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_base_ty = body.local_decls[src_place.local].ty;
                            let shadow_load_ok = self.is_shadowable_ptr_ty(tcx, body, src_ty)
                                && (!matches!(src_ty.kind(), TyKind::Ref(..))
                                    || self.place_contains_deref(src_place)
                                    || !self.is_pointer_ty(src_base_ty));
                            // Projected pointer fields inside non-pointer carriers (for example
                            // `BytesMut.3` or `Option<&T>.0`) should load their leaf shadow
                            // when available. Falling back straight to the carrier-family
                            // reborrow path turns internal raw fields into stack-slot parents.
                            //
                            // Keep the old suppression only for by-value carrier reference
                            // fields: those still rely on the carrier-anchor repair path when
                            // an ABI copy did not reconstruct a concrete leaf shadow slot.
                            let projected_carrier_field_load = !src_place.projection.is_empty()
                                && !self.is_pointer_ty(src_base_ty)
                                && src_place
                                    .projection
                                    .iter()
                                    .all(|pe| !matches!(pe, ProjectionElem::Downcast(..)))
                                && matches!(src_ty.kind(), TyKind::Ref(..));
                            if !projected_carrier_field_load
                                && !src_place.projection.is_empty()
                                && shadow_load_ok
                            {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowLoad dst={:?} src={:?}",
                                    dst_local,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: src_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: self.place_contains_deref(src_place),
                                    },
                                });
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                                return;
                            }
                        }
                    }

                    let mut skip_tag_prop = false;

                    // Preserve lineage for direct projected pointer/reference copies like
                    // `_dst = copy (_agg.1: &mut T)`. Recovering only a base local here is often
                    // too coarse and can leave the destination untagged until first use.
                    let direct_projected_src_place = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op),
                        Rvalue::CopyForDeref(p) => Some(*p),
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute
                            | CastKind::PointerWithExposedProvenance,
                            op,
                            _,
                        ) => self.place_from_operand(op),
                        _ => None,
                    };
                    if let Some(src_place) = direct_projected_src_place {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if !src_place.projection.is_empty()
                            && self.is_pointer_ty(src_ty)
                            && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                        {
                            if self.log_enabled(PassLogLevel::Trace) {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][projected-src] dst={:?} src={:?} dst_ty={:?}",
                                    dst_local,
                                    src_place,
                                    dst_ty
                                );
                            }
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                                    let bk = match dst_ty.kind() {
                                        TyKind::Ref(_, _, Mutability::Mut) => BorrowKind::Mut {
                                            kind: MutBorrowKind::Default,
                                        },
                                        _ => BorrowKind::Shared,
                                    };
                                    InstrKind::Ref {
                                        bk,
                                        src: src_place,
                                        projected_reborrow_anchor_key: None,
                                    }
                                } else {
                                    InstrKind::Raw {
                                        is_mut,
                                        src: src_place,
                                    }
                                },
                            });
                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowStore {
                                        src_local: dst_local,
                                    },
                                });
                            }
                            skip_tag_prop = true;
                        }
                    }

                    // Casts to raw pointers should create a fresh raw tag with parent lineage,
                    // rather than copying the source tag directly.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        if let TyKind::RawPtr(_pointee, mutbl) = dst_ty.kind() {
                            if let Some(src_place) = self.place_from_operand(op) {
                                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                                if self.is_pointer_ty(src_ty) {
                                    let is_mut = matches!(mutbl, Mutability::Mut);
                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_place.local);
                                    tagged_ptr_locals.insert(dst_local);
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place.clone(),
                                        },
                                    });
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
                                    skip_tag_prop = true;
                                }
                            }
                        }
                    }

                    // Pointer-from-non-pointer casts (including exposed provenance and transmute)
                    // drop lineage unless we synthesize a fresh root tag for the destination.
                    if let Rvalue::Cast(
                        CastKind::PtrToPtr
                        | CastKind::PointerCoercion(_, _)
                        | CastKind::Transmute
                        | CastKind::PointerWithExposedProvenance,
                        op,
                        _to_ty,
                    ) = rvalue
                    {
                        let src_ty = op.ty(&body.local_decls, tcx);
                        let explicit_nonnull_transmute =
                            matches!(rvalue, Rvalue::Cast(CastKind::Transmute, ..))
                                && matches!(
                                    src_ty.kind(),
                                    TyKind::Adt(adt, _)
                                        if {
                                            let name = tcx.def_path_str(adt.did());
                                            name.contains("::NonNull") || name.contains("::Unique")
                                        }
                                );
                        if !self.is_pointer_ty(src_ty)
                            && !explicit_nonnull_transmute
                            && self.is_addr_exposable_ptr_ty(tcx, body, dst_ty)
                        {
                            let is_mut = self.ptr_is_mut(dst_ty);
                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            let anchor_key = self.normalized_ptr_expr_key_for_rvalue(
                                body,
                                rvalue,
                                &block_data.statements,
                                stmt_idx,
                            );
                            let reused_anchor_state =
                                anchor_key.as_ref().and_then(|(key, _deps)| {
                                    self.reusable_ssa_anchor_for_expr(
                                        body,
                                        ssa_anchor_for_expr,
                                        key,
                                        dst_local,
                                        dst_ty,
                                    )
                                });
                            if let Some(anchor_state) = reused_anchor_state {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] reuse root-cast dst={:?} anchor={:?} source={:?} key={}",
                                        dst_local,
                                        anchor_state.local,
                                        anchor_state.source,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: match anchor_state.source {
                                        SsaAnchorSource::Tag => InstrKind::TagProp {
                                            dst: dst_local,
                                            src: anchor_state.local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                        SsaAnchorSource::RefAncestor => {
                                            InstrKind::TagPropFromRefAncestor {
                                                dst: dst_local,
                                                src: anchor_state.local,
                                            }
                                        }
                                    },
                                });
                            } else {
                                if trace_ssa_anchor {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ssa-anchor] store root-cast dst={:?} key={}",
                                        dst_local,
                                        anchor_key
                                            .as_ref()
                                            .map(|(key, _)| key.as_str())
                                            .unwrap_or("<none>")
                                    );
                                }
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: true,
                                    },
                                });
                                if let Some((key, deps)) = anchor_key {
                                    ssa_anchor_for_expr.insert(
                                        key,
                                        SsaAnchorState {
                                            local: dst_local,
                                            deps,
                                            source: SsaAnchorSource::Tag,
                                        },
                                    );
                                }
                            }
                            skip_tag_prop = true;
                        }
                    }
                    let src_local_opt: Option<Local> = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        // CopyForDeref shows up when MIR materializes a place for deref;
                        // it still represents a pointer value that needs tag propagation.
                        Rvalue::CopyForDeref(p) => p.as_local(),
                        // Wide-pointer construction can appear as aggregate from (data_ptr, metadata),
                        // e.g. `*mut [T] from (copy _data, copy _len)`. Preserve lineage from field 0.
                        Rvalue::Aggregate(_kind, ops) => ops
                            .iter()
                            .next()
                            .and_then(|op| self.place_from_operand(op))
                            .and_then(|p| p.as_local()),
                        // Pointer arithmetic lowering can appear as `Offset` binary ops.
                        Rvalue::BinaryOp(BinOp::Offset, ops) => {
                            self.place_from_operand(&ops.0).and_then(|p| p.as_local())
                        }
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute,
                            op,
                            _to_ty,
                        ) => self.place_from_operand(op).and_then(|p| p.as_local()),
                        _ => None,
                    };

                    let projected_ptr_place = match rvalue {
                        Rvalue::Use(op) => self.place_from_operand(op),
                        Rvalue::BinaryOp(BinOp::Offset, ops) => self.place_from_operand(&ops.0),
                        Rvalue::Cast(
                            CastKind::PtrToPtr
                            | CastKind::PointerCoercion(_, _)
                            | CastKind::Transmute
                            | CastKind::PointerWithExposedProvenance,
                            op,
                            _,
                        ) => self.place_from_operand(op),
                        _ => None,
                    };

                    let mut src_local_opt = src_local_opt;
                    if src_local_opt.is_none() {
                        if let Some(p) = projected_ptr_place {
                            src_local_opt = self.recover_pointer_source_local_for_projected_place(
                                tcx, body, bb, stmt_idx, p, false,
                            );
                        }
                    }

                    if !skip_tag_prop {
                        let mut handled_ptr_tag = false;
                        if let Some(src_local) = src_local_opt {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                ptr_locals_needing_tag.insert(dst_local);
                                ptr_locals_needing_tag.insert(src_local);

                                let rhs_requires_retag = match rvalue {
                                    // Pointer arithmetic and projection-heavy pointer materialization
                                    // can change the pointee address; copying the source tag directly
                                    // keeps stale pointee metadata. Retag derived values instead.
                                    Rvalue::BinaryOp(op, _)
                                        if matches!(
                                            *op,
                                            BinOp::Offset | BinOp::Add | BinOp::Sub
                                        ) =>
                                    {
                                        true
                                    }
                                    Rvalue::CopyForDeref(_)
                                    | Rvalue::Cast(
                                        CastKind::PtrToPtr
                                        | CastKind::PointerCoercion(_, _)
                                        | CastKind::Transmute,
                                        _,
                                        _,
                                    )
                                    | Rvalue::Aggregate(_, _) => true,
                                    Rvalue::Use(op) => match op {
                                        Operand::Copy(p) | Operand::Move(p) => {
                                            !p.projection.is_empty()
                                        }
                                        _ => false,
                                    },
                                    _ => false,
                                };

                                if rhs_requires_retag {
                                    let anchor_key = projected_ptr_place.and_then(|src_place| {
                                        self.projected_reborrow_anchor_allowed_for_src_place(
                                            body, src_place,
                                        )
                                        .then(|| {
                                            self.normalized_ptr_copy_anchor_key(
                                                tcx,
                                                body,
                                                rvalue,
                                                &block_data.statements,
                                                stmt_idx,
                                            )
                                        })
                                        .flatten()
                                    });
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });

                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse ptr-derive dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store ptr-derive dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::PtrDerive {
                                                dst: dst_local,
                                                src: src_local,
                                                is_mut: self.ptr_is_mut(dst_ty),
                                                is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                                strict_validity: false,
                                            },
                                        });
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::TagProp {
                                            dst: dst_local,
                                            src: src_local,
                                            copy_tag: true,
                                            copy_ref_ancestor: true,
                                        },
                                    });
                                    self.rebind_ssa_anchors_for_copy(
                                        ssa_anchor_for_expr,
                                        src_local,
                                        dst_local,
                                    );
                                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::PtrUse {
                                                ptr_local: dst_local,
                                            },
                                        });
                                    }
                                }
                                tagged_ptr_locals.insert(dst_local);
                                if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::ShadowStore {
                                            src_local: dst_local,
                                        },
                                    });
                                }
                                handled_ptr_tag = true;
                            }
                        }
                        if !handled_ptr_tag {
                            // If the RHS is a projected place, there may be no pointer local we can
                            // propagate from, but the destination still needs a tag for later derefs.
                            // Synthesize a fresh root tag so the runtime does not see UNKNOWN_TAG.
                            let rhs_is_projected_ptr = match rvalue {
                                Rvalue::Use(op) => match op {
                                    Operand::Copy(p) | Operand::Move(p) => {
                                        self.is_pointer_ty(p.ty(&body.local_decls, tcx).ty)
                                    }
                                    _ => false,
                                },
                                Rvalue::CopyForDeref(_p) => {
                                    // If CopyForDeref produces a thin pointer local but we cannot
                                    // propagate from a source local, synthesize a root tag for it.
                                    // The destination type check below guards against non-pointers.
                                    true
                                }
                                _ => false,
                            };

                            if matches!(rvalue, Rvalue::CopyForDeref(_))
                                && self.log_enabled(PassLogLevel::Trace)
                            {
                                rz_pass_trace!(
                                    self,
                                    "CopyForDeref dst_local={:?} dst_ty={:?} rhs_is_projected_ptr={}",
                                    dst_local,
                                    dst_ty,
                                    rhs_is_projected_ptr
                                );
                            }

                            if rhs_is_projected_ptr {
                                ptr_locals_needing_tag.insert(dst_local);
                                // The destination local is initialized by this assignment.
                                // Synthesize a root tag *after* the assignment so later uses
                                // (including call operands) do not see UNKNOWN_TAG. Preserve
                                // reference semantics for projected `&T` / `&mut T` copies;
                                // rooting them as raw pointers loses ref-kind behavior and can
                                // desynchronize later access metadata from the actual pointee.
                                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                    let is_mut = self.ptr_is_mut(dst_ty);
                                    let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                                    let projected_src_place = match rvalue {
                                        Rvalue::Use(op) => self.place_from_operand(op),
                                        Rvalue::CopyForDeref(p) => Some(*p),
                                        Rvalue::Cast(
                                            CastKind::PtrToPtr
                                            | CastKind::PointerCoercion(_, _)
                                            | CastKind::Transmute
                                            | CastKind::PointerWithExposedProvenance,
                                            op,
                                            _,
                                        ) => self.place_from_operand(op),
                                        _ => None,
                                    };
                                    let anchor_key = self.normalized_ptr_copy_anchor_key(
                                        tcx,
                                        body,
                                        rvalue,
                                        &block_data.statements,
                                        stmt_idx,
                                    );
                                    let reused_anchor_state =
                                        anchor_key.as_ref().and_then(|(key, _deps)| {
                                            self.reusable_ssa_anchor_for_expr(
                                                body,
                                                ssa_anchor_for_expr,
                                                key,
                                                dst_local,
                                                dst_ty,
                                            )
                                        });
                                    if let Some(anchor_state) = reused_anchor_state {
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] reuse projected-root dst={:?} anchor={:?} source={:?} key={}",
                                                dst_local,
                                                anchor_state.local,
                                                anchor_state.source,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: match anchor_state.source {
                                                SsaAnchorSource::Tag => InstrKind::TagProp {
                                                    dst: dst_local,
                                                    src: anchor_state.local,
                                                    copy_tag: true,
                                                    copy_ref_ancestor: true,
                                                },
                                                SsaAnchorSource::RefAncestor => {
                                                    InstrKind::TagPropFromRefAncestor {
                                                        dst: dst_local,
                                                        src: anchor_state.local,
                                                    }
                                                }
                                            },
                                        });
                                    } else {
                                        let projected_reborrow_anchor_key = projected_src_place
                                            .and_then(|src_place| {
                                                self.maybe_projected_reborrow_anchor_key(
                                                    body,
                                                    src_place,
                                                    anchor_key.as_ref(),
                                                    projected_reborrow_anchor_specs,
                                                )
                                            });
                                        if trace_ssa_anchor {
                                            rz_pass_trace!(
                                                self,
                                                "[rusteze][ssa-anchor] store projected-root dst={:?} key={}",
                                                dst_local,
                                                anchor_key
                                                    .as_ref()
                                                    .map(|(key, _)| key.as_str())
                                                    .unwrap_or("<none>")
                                            );
                                        }
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: if let Some(src_place) = projected_src_place {
                                                if is_ref {
                                                    let bk = match dst_ty.kind() {
                                                        TyKind::Ref(_, _, Mutability::Mut) => {
                                                            BorrowKind::Mut {
                                                                kind: MutBorrowKind::Default,
                                                            }
                                                        }
                                                        _ => BorrowKind::Shared,
                                                    };
                                                    InstrKind::Ref {
                                                        bk,
                                                        src: src_place,
                                                        projected_reborrow_anchor_key,
                                                    }
                                                } else {
                                                    InstrKind::Raw {
                                                        is_mut,
                                                        src: src_place,
                                                    }
                                                }
                                            } else if is_ref {
                                                InstrKind::RetRoot {
                                                    dst_local,
                                                    is_mut,
                                                    is_ref: true,
                                                }
                                            } else {
                                                InstrKind::RawRoot {
                                                    ptr_local: dst_local,
                                                    is_mut,
                                                    exposed_provenance: matches!(
                                                        rvalue,
                                                        Rvalue::Cast(
                                                            CastKind::PointerWithExposedProvenance,
                                                            ..,
                                                        )
                                                    ),
                                                }
                                            },
                                        });
                                        if let Some((key, deps)) = anchor_key {
                                            ssa_anchor_for_expr.insert(
                                                key,
                                                SsaAnchorState {
                                                    local: dst_local,
                                                    deps,
                                                    source: SsaAnchorSource::Tag,
                                                },
                                            );
                                        }
                                    }
                                    tagged_ptr_locals.insert(dst_local);
                                    if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ShadowStore {
                                                src_local: dst_local,
                                            },
                                        });
                                    }
                                }
                            } else {
                                // If the RHS is a global/promoted pointer constant, record its allocation
                                // and synthesize a root tag for the destination.
                                let const_op: Option<&ConstOperand<'tcx>> = match rvalue {
                                    Rvalue::Use(Operand::Constant(c)) => Some(c),
                                    Rvalue::Cast(_, op, _) => match op {
                                        Operand::Constant(c) => Some(c),
                                        _ => None,
                                    },
                                    _ => None,
                                };

                                if let Some(c) = const_op {
                                    if let Some(info) = self.const_alloc_info(tcx, c) {
                                        let is_mut = self.ptr_is_mut(dst_ty);

                                        ptr_locals_needing_tag.insert(dst_local);
                                        insert_points.push(InsertPoint {
                                            bb,
                                            stmt_idx,
                                            insert_before: false,
                                            source_info: stmt.source_info,
                                            place: Place::from(dst_local),
                                            kind: InstrKind::ConstAlloc {
                                                ptr_local: dst_local,
                                                size: info.size,
                                                base_offset: info.base_offset,
                                            },
                                        });
                                        if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                                            insert_points.push(InsertPoint {
                                                bb,
                                                stmt_idx: stmt_idx + 1,
                                                insert_before: false,
                                                source_info: stmt.source_info,
                                                place: Place::from(dst_local),
                                                kind: InstrKind::RawRoot {
                                                    ptr_local: dst_local,
                                                    is_mut,
                                                    exposed_provenance: false,
                                                },
                                            });
                                            tagged_ptr_locals.insert(dst_local);
                                            if self.is_shadowable_ptr_ty(tcx, body, dst_ty) {
                                                insert_points.push(InsertPoint {
                                                    bb,
                                                    stmt_idx,
                                                    insert_before: false,
                                                    source_info: stmt.source_info,
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
                    }
                }
            }
        }

        // Pointer-shadow maintenance for memory-resident pointer slots.
        if let StatementKind::Assign(box (lhs_place, rvalue)) = &stmt.kind {
            let lhs_ty = lhs_place.ty(&body.local_decls, tcx).ty;

            if !lhs_place.projection.is_empty() {
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place)) = rvalue
                {
                    if src_place.ty(&body.local_decls, tcx).ty == lhs_ty {
                        let lhs_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, lhs_ty, 3);
                        let dst_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *lhs_place, lhs_ty);
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, lhs_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowStore dst_field={:?} src_local={:?}",
                                        dst_field,
                                        src_field.local
                                    );
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    rz_pass_trace!(
                                        self,
                                        "[rusteze][ptr-shadow] Projected Aggregate ShadowCopySlot dst_field={:?} src={:?}",
                                        dst_field,
                                        src_field
                                    );
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                            return;
                        }
                        if lhs_has_ptr_fields && lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Projected Aggregate ShadowCopyRange dst={:?} src={:?}",
                                lhs_place,
                                src_place
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                            return;
                        }
                    }
                }

                if self.place_contains_deref(lhs_place.clone())
                    && self.is_one_byte_sized_ty(tcx, lhs_ty)
                {
                    let byte_copy_src = match rvalue {
                        Rvalue::Use(Operand::Copy(src_place))
                        | Rvalue::Use(Operand::Move(src_place)) => src_place
                            .as_local()
                            .and_then(|src_local| byte_copy_src_for_local.get(&src_local).copied()),
                        _ => None,
                    };
                    if let Some(src_place) = byte_copy_src {
                        rz_pass_trace!(
                            self,
                            "[rusteze][ptr-shadow] ShadowCopyRange dst={:?} src={:?} size=1",
                            lhs_place,
                            src_place
                        );
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowCopyRange {
                                src_place,
                                size_op: SizeOperand::Const(self.const_usize(
                                    tcx,
                                    stmt.source_info.span,
                                    1,
                                )),
                            },
                        });
                        return;
                    }
                }

                if self.is_shadowable_ptr_ty(tcx, body, lhs_ty) {
                    if let Some(src_place) = self.pointer_place_from_rvalue(rvalue) {
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if self.is_shadowable_ptr_ty(tcx, body, src_ty) {
                            if src_place.projection.is_empty() {
                                ptr_locals_needing_tag.insert(src_place.local);
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowStore dst={:?} src_local={:?}",
                                    lhs_place,
                                    src_place.local
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowStore {
                                        src_local: src_place.local,
                                    },
                                });
                            } else {
                                rz_pass_trace!(
                                    self,
                                    "[rusteze][ptr-shadow] ShadowCopySlot dst={:?} src={:?}",
                                    lhs_place,
                                    src_place
                                );
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: lhs_place.clone(),
                                    kind: InstrKind::ShadowCopySlot { src_place },
                                });
                            }
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: lhs_place.clone(),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        lhs_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    } else {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: lhs_place.clone(),
                            kind: InstrKind::ShadowKill {
                                size_op: self.size_operand_for_ty(
                                    tcx,
                                    body,
                                    lhs_ty,
                                    stmt.source_info.span,
                                ),
                            },
                        });
                    }
                } else if lhs_ty.is_sized(tcx, body.typing_env(tcx)) {
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: lhs_place.clone(),
                        kind: InstrKind::ShadowKill {
                            size_op: self.size_operand_for_ty(
                                tcx,
                                body,
                                lhs_ty,
                                stmt.source_info.span,
                            ),
                        },
                    });
                }
            } else if let Some(dst_local) = lhs_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if let Rvalue::Use(Operand::Copy(src_place))
                | Rvalue::Use(Operand::Move(src_place))
                | Rvalue::CopyForDeref(src_place) = rvalue
                {
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx,
                                body,
                                src_place.local,
                                dst_ty,
                            )
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                        }
                    }
                }
                match rvalue {
                    Rvalue::Aggregate(kind, ops) => {
                        let ops_vec: Vec<Operand<'tcx>> = ops.iter().cloned().collect();
                        self.emit_aggregate_shadow_ops(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            stmt.source_info,
                            dst_local,
                            dst_ty,
                            kind,
                            &ops_vec,
                            insert_points,
                            ptr_locals_needing_tag,
                        );
                    }
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place))
                    | Rvalue::CopyForDeref(src_place)
                        if src_place.ty(&body.local_decls, tcx).ty == dst_ty =>
                    {
                        let dst_has_ptr_fields =
                            self.ty_contains_pointer_fields(tcx, body, dst_ty, 3);
                        let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(dst_local),
                            dst_ty,
                        );
                        let src_leafs = self
                            .shadowable_leaf_ptr_specs_from_place(tcx, body, *src_place, dst_ty);
                        if let Some(matched_leafs) =
                            self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                        {
                            for (dst_spec, src_spec) in matched_leafs {
                                let dst_field = dst_spec.place;
                                let src_field = src_spec.place;
                                let kind = if src_field.projection.is_empty()
                                    && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                {
                                    InstrKind::ShadowStore {
                                        src_local: src_field.local,
                                    }
                                } else {
                                    InstrKind::ShadowCopySlot {
                                        src_place: src_field,
                                    }
                                };
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: dst_field,
                                    kind,
                                });
                            }
                        } else if dst_has_ptr_fields && dst_ty.is_sized(tcx, body.typing_env(tcx)) {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowCopyRange {
                                    src_place: *src_place,
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        } else {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ShadowKill {
                                    size_op: self.size_operand_for_ty(
                                        tcx,
                                        body,
                                        dst_ty,
                                        stmt.source_info.span,
                                    ),
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Wrapper-carrier transmute: optimized MIR frequently wraps a pointer local into a
        // `NonNull<T>`/`Unique<T>`-style carrier via `Transmute`. Restore the destination leaf
        // shadow so later projected loads from the wrapper field do not degrade to UNKNOWN_TAG.
        if let StatementKind::Assign(box (dst_place, Rvalue::Cast(CastKind::Transmute, op, _))) =
            &stmt.kind
        {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    let dst_leafs = self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    );
                    if !dst_leafs.is_empty() {
                        if let Some(src_place) = self.place_from_operand(op) {
                            let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                            let src_leafs = self
                                .shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, src_ty);
                            if let Some(matched_leafs) =
                                self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                            {
                                for (dst_spec, src_spec) in matched_leafs {
                                    let dst_leaf = dst_spec.place;
                                    let src_leaf = src_spec.place;
                                    let kind = if src_leaf.projection.is_empty()
                                        && self.is_shadowable_ptr_ty(tcx, body, dst_spec.ty)
                                    {
                                        ptr_locals_needing_tag.insert(src_leaf.local);
                                        InstrKind::ShadowStore {
                                            src_local: src_leaf.local,
                                        }
                                    } else {
                                        InstrKind::ShadowCopySlot {
                                            src_place: src_leaf,
                                        }
                                    };
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: dst_leaf,
                                        kind,
                                    });
                                }
                            } else if self.ty_contains_pointer_fields(tcx, body, src_ty, 3)
                                && dst_ty.is_sized(tcx, body.typing_env(tcx))
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::ShadowKill {
                                        size_op: self.size_operand_for_ty(
                                            tcx,
                                            body,
                                            dst_ty,
                                            stmt.source_info.span,
                                        ),
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Handle pointer constants embedded in aggregate/field assignments where the destination
        // is not a pointer local we can tag. We still want to record the backing global allocation
        // so later derefs (e.g., vtable loads) do not report WILD_POINTER.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            let dst_is_local = dst_place.as_local().is_some();
            let mut const_ops: Vec<&ConstOperand<'tcx>> = Vec::new();

            match rvalue {
                Rvalue::Aggregate(_, ops) => {
                    for op in ops.iter() {
                        if let Operand::Constant(c) = op {
                            const_ops.push(c);
                        }
                    }
                }
                Rvalue::Use(Operand::Constant(c)) | Rvalue::Cast(_, Operand::Constant(c), _) => {
                    if !dst_is_local {
                        const_ops.push(c);
                    }
                }
                _ => {}
            }

            if !const_ops.is_empty() {
                let place_local = dst_place.as_local().unwrap_or(RETURN_PLACE);
                for c in const_ops {
                    if let Some(info) = self.const_alloc_info(tcx, c) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(place_local),
                            kind: InstrKind::ConstAllocConst {
                                const_op: c.clone(),
                                size: info.size,
                                base_offset: info.base_offset,
                            },
                        });
                    }
                }
            }
        }

        // std/alloc pattern where a thin pointer is produced by `Transmute` from
        // `NonNull<T>`/`Unique<T>` (ADT). TagProp does not apply because the source is not
        // necessarily a thin pointer local. Prefer deriving from a recovered pointer source
        // local (to keep lineage), and fall back to RawRoot only when recovery fails.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    if let Rvalue::Cast(CastKind::Transmute, op, _to_ty) = rvalue {
                        let src_ty = op.ty(body, tcx);
                        let is_nonnull_like = match src_ty.kind() {
                            TyKind::Adt(adt, _) => {
                                let name = tcx.def_path_str(adt.did());
                                name.contains("::NonNull") || name.contains("::Unique")
                            }
                            _ => false,
                        };

                        if is_nonnull_like {
                            let is_mut = match dst_ty.kind() {
                                TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                                _ => false,
                            };
                            let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));
                            let src_place_opt = self.place_from_operand(op);
                            let direct_field_shadow = src_place_opt
                                .and_then(|src_place| src_place.as_local())
                                .and_then(|src_local| {
                                    self.first_direct_pointer_field_place(tcx, body, src_local)
                                });
                            if let Some(field_place) = direct_field_shadow {
                                ptr_locals_needing_tag.insert(dst_local);
                                tagged_ptr_locals.insert(dst_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: field_place,
                                    kind: InstrKind::ShadowLoad {
                                        dst_local,
                                        require_tag: false,
                                        validate_ref: false,
                                    },
                                });
                                return;
                            }
                            let src_local_opt = src_place_opt
                                .and_then(|src_place| {
                                    if matches!(
                                        rvalue,
                                        Rvalue::Cast(CastKind::PointerWithExposedProvenance, ..,)
                                    ) {
                                        self.backtrack_global_exposed_provenance_source_local(
                                            body, dst_local,
                                        )
                                        .or_else(|| {
                                            src_place.as_local().and_then(|src_local| {
                                                if self
                                                    .is_pointer_ty(body.local_decls[src_local].ty)
                                                {
                                                    Some(src_local)
                                                } else {
                                                    self.backtrack_pointer_source_local(
                                                        body,
                                                        src_local,
                                                        &block_data.statements[..stmt_idx],
                                                    )
                                                    .or_else(|| {
                                                        self.backtrack_global_pointer_value_local(
                                                            body, src_local,
                                                        )
                                                    })
                                                }
                                            })
                                        })
                                    } else if let Some(src_local) = src_place.as_local() {
                                        if self.is_pointer_ty(body.local_decls[src_local].ty) {
                                            Some(src_local)
                                        } else {
                                            self.backtrack_pointer_source_local(
                                                body,
                                                src_local,
                                                &block_data.statements[..stmt_idx],
                                            )
                                            .or_else(
                                                || {
                                                    self.backtrack_global_pointer_value_local(
                                                        body, src_local,
                                                    )
                                                },
                                            )
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .and_then(|src_local| {
                                    let Some(src_place) = src_place_opt else {
                                        return Some(src_local);
                                    };
                                    let src_place_ty = src_place.ty(&body.local_decls, tcx).ty;
                                    let projected_carrier_load = !self.is_pointer_ty(src_place_ty)
                                        && !src_place.projection.is_empty()
                                        && src_local == src_place.local;
                                    if projected_carrier_load {
                                        None
                                    } else {
                                        Some(src_local)
                                    }
                                });

                            ptr_locals_needing_tag.insert(dst_local);
                            tagged_ptr_locals.insert(dst_local);
                            if let Some(src_local) = src_local_opt {
                                ptr_locals_needing_tag.insert(src_local);
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::PtrDerive {
                                        dst: dst_local,
                                        src: src_local,
                                        is_mut,
                                        is_ref,
                                        strict_validity: false,
                                    },
                                });
                            } else if let Some(src_place) = src_place_opt {
                                if !is_ref {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::Raw {
                                            is_mut,
                                            src: src_place,
                                        },
                                    });
                                } else {
                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::RawRoot {
                                            ptr_local: dst_local,
                                            is_mut,
                                            exposed_provenance: false,
                                        },
                                    });
                                }
                            } else {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info: stmt.source_info,
                                    place: Place::from(dst_local),
                                    kind: InstrKind::RawRoot {
                                        ptr_local: dst_local,
                                        is_mut,
                                        exposed_provenance: false,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Fresh tag on pointer arithmetic: derived pointers (add/sub/offset) get a new tag
        // with parent linkage to the base pointer tag.
        if let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind {
            if let Some(dst_local) = dst_place.as_local() {
                let dst_ty = body.local_decls[dst_local].ty;
                if self.is_addr_exposable_ptr_ty(tcx, body, dst_ty) {
                    let (binop, lhs_op) = match rvalue {
                        Rvalue::BinaryOp(op, box (lhs, _rhs)) => (Some(*op), Some(lhs)),
                        // Newer nightlies no longer have `Rvalue::CheckedBinaryOp`. The checked/overflowing
                        // forms lower to regular `BinaryOp` + extra logic, so handling `BinaryOp` is enough
                        // for our pointer-derive tagging purposes here.
                        _ => (None, None),
                    };

                    if let (Some(op), Some(lhs)) = (binop, lhs_op) {
                        // Raw pointer arithmetic frequently lowers to `BinOp::Offset`.
                        // Treat it like Add/Sub for tag-derivation so the derived pointer
                        // local does not remain untagged.
                        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Offset) {
                            if let Some(src_place) = self.place_from_operand(lhs) {
                                let src_local = src_place.local;
                                let src_ty = body.local_decls[src_local].ty;
                                if self.is_thin_ptr_ty(tcx, body, src_ty) {
                                    let is_mut = match dst_ty.kind() {
                                        TyKind::Ref(_, _ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
                                        TyKind::RawPtr(_ty, mutbl) => {
                                            matches!(mutbl, Mutability::Mut)
                                        }
                                        _ => false,
                                    };

                                    ptr_locals_needing_tag.insert(dst_local);
                                    ptr_locals_needing_tag.insert(src_local);
                                    tagged_ptr_locals.insert(dst_local);

                                    insert_points.push(InsertPoint {
                                        bb,
                                        stmt_idx,
                                        insert_before: false,
                                        source_info: stmt.source_info,
                                        place: Place::from(dst_local),
                                        kind: InstrKind::PtrDerive {
                                            dst: dst_local,
                                            src: src_local,
                                            is_mut,
                                            is_ref: matches!(dst_ty.kind(), TyKind::Ref(..)),
                                            strict_validity: true,
                                        },
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Ref creation
        if let StatementKind::Assign(box (place, Rvalue::Ref(_, bk, src_place))) = &stmt.kind {
            if let Some(lhs_local) = place.as_local() {
                let lhs_ty = body.local_decls[lhs_local].ty;
                let black_box_sink_ref_temp = if matches!(bk, BorrowKind::Shared)
                    && stmt_idx + 1 == block_data.statements.len()
                {
                    if let Some(Terminator {
                        kind:
                            TerminatorKind::Call {
                                func,
                                args,
                                destination,
                                ..
                            },
                        ..
                    }) = &block_data.terminator
                    {
                        let call_dest_loc = Location {
                            block: bb,
                            statement_index: block_data.statements.len(),
                        };
                        let call_returns_dead = destination.as_local().is_some_and(|local| {
                            !self.local_has_observable_use_excluding_call_dest(
                                body,
                                local,
                                call_dest_loc,
                            )
                        });
                        let call_is_black_box = self
                            .direct_callee(tcx, body, block_data, func)
                            .map(|(did, _)| tcx.def_path_str(did).contains("black_box"))
                            .unwrap_or(false);
                        let arg_matches = args.iter().any(|arg| {
                            self.place_from_operand(&arg.node)
                                .is_some_and(|arg_place| arg_place.as_local() == Some(lhs_local))
                        });
                        call_returns_dead && call_is_black_box && arg_matches
                    } else {
                        false
                    }
                } else {
                    false
                };
                if self.is_pointer_ty(lhs_ty)
                    && !summary_elidable_shared_call_ref_locals.contains(&lhs_local)
                    && !black_box_sink_ref_temp
                {
                    let allow_projected_anchor =
                        self.projected_reborrow_anchor_allowed_for_src_place(body, *src_place);
                    let anchor_key = allow_projected_anchor
                        .then(|| {
                            self.normalized_ptr_expr_key_for_ref_source_place(
                                body,
                                *src_place,
                                &block_data.statements,
                                stmt_idx,
                            )
                        })
                        .flatten();
                    let reused_anchor_state = if allow_projected_anchor
                        && self.allow_ssa_anchor_reuse_for_ref_source_place(body, *bk, *src_place)
                    {
                        anchor_key.as_ref().and_then(|(key, _deps)| {
                            self.reusable_ssa_anchor_for_ref_source_expr(
                                body,
                                ssa_anchor_for_expr,
                                key,
                                lhs_local,
                            )
                        })
                    } else {
                        None
                    };
                    let ref_src_place = reused_anchor_state
                        .as_ref()
                        .map(|anchor_state| Place::from(anchor_state.local))
                        .unwrap_or(*src_place);
                    let projected_reborrow_anchor_key = self.maybe_projected_reborrow_anchor_key(
                        body,
                        *src_place,
                        anchor_key.as_ref(),
                        projected_reborrow_anchor_specs,
                    );

                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Ref {
                            bk: *bk,
                            src: ref_src_place,
                            projected_reborrow_anchor_key,
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    if let Some(anchor_state) = reused_anchor_state.as_ref() {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] reuse ref parent dst={:?} anchor={:?} source={:?} key={}",
                                lhs_local,
                                anchor_state.local,
                                anchor_state.source,
                                anchor_key
                                    .as_ref()
                                    .map(|(key, _)| key.as_str())
                                    .unwrap_or("<none>")
                            );
                        }
                    }
                    if let Some((key, deps)) = anchor_key {
                        if trace_ssa_anchor {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ssa-anchor] store ref-ancestor dst={:?} key={}",
                                lhs_local,
                                key
                            );
                        }
                        ssa_anchor_for_expr.insert(
                            key,
                            SsaAnchorState {
                                local: lhs_local,
                                deps,
                                source: SsaAnchorSource::RefAncestor,
                            },
                        );
                    }
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
                }
            }
        }

        // Raw pointer creation
        if let StatementKind::Assign(box (place, Rvalue::RawPtr(mutbl, src_place))) = &stmt.kind {
            if let Some(lhs_local) = place.as_local() {
                let lhs_ty = body.local_decls[lhs_local].ty;
                if self.is_pointer_ty(lhs_ty) {
                    ptr_locals_needing_tag.insert(lhs_local);
                    tagged_ptr_locals.insert(lhs_local);
                    let is_mut = matches!(*mutbl, RawPtrKind::Mut);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::Raw {
                            is_mut,
                            src: src_place.clone(),
                        },
                    });
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info: stmt.source_info,
                        place: place.clone(),
                        kind: InstrKind::ShadowStore {
                            src_local: lhs_local,
                        },
                    });
                    let src_local = src_place.local;
                    let src_begins_with_deref = src_place
                        .projection
                        .first()
                        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref));
                    if interesting_stack_locals.contains(&src_local)
                        && !self.is_pointer_ty(body.local_decls[src_local].ty)
                        && !src_begins_with_deref
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(src_local),
                            kind: InstrKind::ReborrowAnchorSeed {
                                dst_local: src_local,
                                src_local: lhs_local,
                                mark_slot_family: false,
                            },
                        });
                    }
                }
            }
        }
    }

    fn operand_mentions_local<'tcx>(&self, operand: &Operand<'tcx>, local: Local) -> bool {
        self.place_from_operand(operand)
            .and_then(|place| place.as_local())
            == Some(local)
    }

    fn should_skip_storage_dead_shadow_kill<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        block_data: &BasicBlockData<'tcx>,
        stmt_idx: usize,
        local: Local,
    ) -> bool {
        let Some(prev_stmt) = stmt_idx
            .checked_sub(1)
            .and_then(|idx| block_data.statements.get(idx))
        else {
            return false;
        };
        let StatementKind::Assign(box (dst_place, rvalue)) = &prev_stmt.kind else {
            return false;
        };
        if dst_place.as_local() == Some(local) {
            return false;
        }
        let dst_ty = dst_place.ty(&body.local_decls, tcx).ty;
        if !self.is_shadowable_ptr_ty(tcx, body, dst_ty)
            && !self.ty_contains_pointer_fields(tcx, body, dst_ty, 8)
        {
            return false;
        }
        match rvalue {
            Rvalue::Aggregate(_, ops) => {
                ops.iter().any(|op| self.operand_mentions_local(op, local))
            }
            Rvalue::Use(op) | Rvalue::Cast(_, op, _) => self.operand_mentions_local(op, local),
            Rvalue::CopyForDeref(place) => place.as_local() == Some(local),
            _ => false,
        }
    }

    fn aggregate_field_specs_for_kind<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        dst_ty: Ty<'tcx>,
        aggregate_kind: &AggregateKind<'tcx>,
    ) -> Option<(Option<VariantIdx>, Vec<(usize, Ty<'tcx>)>)> {
        match (dst_ty.kind(), aggregate_kind) {
            (TyKind::Tuple(field_tys), AggregateKind::Tuple) => Some((
                None,
                field_tys
                    .iter()
                    .enumerate()
                    .map(|(idx, field_ty)| (idx, field_ty))
                    .collect(),
            )),
            (TyKind::Adt(adt, args), AggregateKind::Adt(_, variant_idx, _, _, active_field)) => {
                let variant = &adt.variant(*variant_idx);
                if let Some(active_field) = *active_field {
                    let field_ty = variant.fields[active_field].ty(tcx, args);
                    Some((
                        Some(*variant_idx),
                        vec![(active_field.as_usize(), field_ty)],
                    ))
                } else {
                    Some((
                        Some(*variant_idx),
                        variant
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(idx, field)| (idx, field.ty(tcx, args)))
                            .collect(),
                    ))
                }
            }
            _ => None,
        }
    }

    fn emit_aggregate_shadow_ops<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        stmt_idx: usize,
        source_info: SourceInfo,
        dst_local: Local,
        dst_ty: Ty<'tcx>,
        aggregate_kind: &AggregateKind<'tcx>,
        ops: &[Operand<'tcx>],
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
    ) {
        let Some((variant, field_specs)) =
            self.aggregate_field_specs_for_kind(tcx, dst_ty, aggregate_kind)
        else {
            return;
        };

        for (field_idx, field_ty) in field_specs.into_iter() {
            if !self.is_shadowable_ptr_ty(tcx, body, field_ty)
                && !self.ty_contains_pointer_fields(tcx, body, field_ty, 3)
            {
                continue;
            }
            let Some(op) = ops.get(field_idx) else {
                continue;
            };
            let field_place = self.pointer_field_place_from_place_in_variant(
                tcx,
                Place::from(dst_local),
                variant,
                field_idx,
                field_ty,
            );
            let dst_leafs =
                self.shadowable_leaf_ptr_specs_from_place(tcx, body, field_place, field_ty);
            if dst_leafs.is_empty() {
                continue;
            }
            if let Some(src_place) = self.place_from_operand(op) {
                let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                let src_leafs =
                    self.shadowable_leaf_ptr_specs_from_place(tcx, body, src_place, src_ty);
                if let Some(matched_leafs) =
                    self.pair_shadowable_leaf_ptr_specs(&dst_leafs, &src_leafs)
                {
                    for (dst_spec, src_spec) in matched_leafs {
                        let dst_leaf = dst_spec.place;
                        let dst_leaf_ty = dst_spec.ty;
                        let src_leaf = src_spec.place;
                        if src_leaf.projection.is_empty()
                            && self.is_shadowable_ptr_ty(tcx, body, dst_leaf_ty)
                        {
                            ptr_locals_needing_tag.insert(src_leaf.local);
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Aggregate ShadowStore dst_field={:?} src_local={:?}",
                                dst_leaf,
                                src_leaf.local
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info,
                                place: dst_leaf,
                                kind: InstrKind::ShadowStore {
                                    src_local: src_leaf.local,
                                },
                            });
                        } else {
                            rz_pass_trace!(
                                self,
                                "[rusteze][ptr-shadow] Aggregate ShadowCopySlot dst_field={:?} src={:?}",
                                dst_leaf,
                                src_leaf
                            );
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info,
                                place: dst_leaf,
                                kind: InstrKind::ShadowCopySlot {
                                    src_place: src_leaf,
                                },
                            });
                        }
                    }
                    continue;
                }
                if field_ty.is_sized(tcx, body.typing_env(tcx)) {
                    rz_pass_trace!(
                        self,
                        "[rusteze][ptr-shadow] Aggregate ShadowCopyRange dst_field={:?} src={:?}",
                        field_place,
                        src_place
                    );
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx,
                        insert_before: false,
                        source_info,
                        place: field_place,
                        kind: InstrKind::ShadowCopyRange {
                            src_place,
                            size_op: self.size_operand_for_ty(
                                tcx,
                                body,
                                field_ty,
                                source_info.span,
                            ),
                        },
                    });
                    continue;
                }
            }

            for dst_leaf_spec in dst_leafs {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info,
                    place: dst_leaf_spec.place,
                    kind: InstrKind::ShadowKill {
                        size_op: self.size_operand_for_ty(
                            tcx,
                            body,
                            dst_leaf_spec.ty,
                            source_info.span,
                        ),
                    },
                });
            }
        }
    }

    /// Best-effort: compute byte size operand for memory ops given a pointer local and a count operand.
    ///
    /// Semantics: byte_len = count * size_of::<T>(), where `count` is in *elements*.
    ///
    /// Policy:
    /// - For thin pointers to sized types, emit `count * size_of::<T>()` as a MIR expression.
    /// - Otherwise, return a constant 0 (unknown).
    fn memop_size_bytes<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        ptr_local: Local,
        count_op: &Operand<'tcx>,
        span: Span,
    ) -> SizeOperand<'tcx> {
        let ptr_ty = body.local_decls[ptr_local].ty;
        if !self.is_thin_ptr_ty(tcx, body, ptr_ty) {
            return self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, span);
        }

        let elem_ty = match ptr_ty.kind() {
            TyKind::RawPtr(pointee_ty, _) => *pointee_ty,
            TyKind::Ref(_, pointee_ty, _) => *pointee_ty,
            _ => {
                return SizeOperand::Const(self.const_usize(tcx, span, 0));
            }
        };

        if !elem_ty.is_sized(tcx, body.typing_env(tcx)) {
            // TODO(wide-ptr): support unsized element types by using metadata length when available.
            return SizeOperand::Const(self.const_usize(tcx, span, 0));
        }

        SizeOperand::ElemCount {
            elem_ty,
            count_op: count_op.clone(),
        }
    }

    fn materialize_size_operand<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        source_info: SourceInfo,
        size_op: &SizeOperand<'tcx>,
    ) -> (Operand<'tcx>, Vec<Statement<'tcx>>) {
        match size_op {
            SizeOperand::Const(op) => (op.clone(), Vec::new()),
            SizeOperand::SizeOf(ty) => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(size_local)), vec![stmt])
            }
            SizeOperand::RefSizedBoundsLen(ty) => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_usize_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let zst_marker_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let encoded_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *ty),
                    ))),
                );
                let is_zero_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(is_zero_local),
                        Rvalue::BinaryOp(
                            BinOp::Eq,
                            Box::new((
                                Operand::Copy(Place::from(size_local)),
                                self.const_usize(tcx, source_info.span, 0),
                            )),
                        ),
                    ))),
                );
                let is_zero_usize_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(is_zero_usize_local),
                        Rvalue::Cast(
                            CastKind::IntToInt,
                            Operand::Copy(Place::from(is_zero_local)),
                            tcx.types.usize,
                        ),
                    ))),
                );
                let zst_marker_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(zst_marker_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(is_zero_usize_local)),
                                self.const_usize(tcx, source_info.span, usize::MAX - 1),
                            )),
                        ),
                    ))),
                );
                let encoded_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(encoded_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(size_local)),
                                Operand::Copy(Place::from(zst_marker_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(encoded_local)),
                    vec![
                        size_stmt,
                        is_zero_stmt,
                        is_zero_usize_stmt,
                        zst_marker_stmt,
                        encoded_stmt,
                    ],
                )
            }
            SizeOperand::AlignOf(ty) => {
                let align_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(align_local),
                        Rvalue::NullaryOp(NullOp::AlignOf, *ty),
                    ))),
                );
                (Operand::Copy(Place::from(align_local)), vec![stmt])
            }
            SizeOperand::ElemCount { elem_ty, count_op } => {
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((Operand::Copy(Place::from(size_local)), count_op.clone())),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataSlice { ptr_local, elem_ty } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                let size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(size_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(bytes_local)),
                    vec![meta_stmt, size_stmt, bytes_stmt],
                )
            }
            SizeOperand::PtrMetadataStr { ptr_local } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                (Operand::Copy(Place::from(meta_local)), vec![meta_stmt])
            }
            SizeOperand::PtrMetadataAdtSlice {
                ptr_local,
                adt_ty,
                field_idx,
                elem_ty,
            } => {
                let meta_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let elem_size_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let tail_bytes_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let head_off_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let total_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let meta_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(meta_local),
                        Rvalue::UnaryOp(UnOp::PtrMetadata, Operand::Copy(Place::from(*ptr_local))),
                    ))),
                );
                let elem_size_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(elem_size_local),
                        Rvalue::NullaryOp(NullOp::SizeOf, *elem_ty),
                    ))),
                );
                let tail_bytes_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(tail_bytes_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(meta_local)),
                                Operand::Copy(Place::from(elem_size_local)),
                            )),
                        ),
                    ))),
                );
                let offset_of_list = tcx.mk_offset_of(&[(VariantIdx::from_u32(0), *field_idx)]);
                let head_off_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(head_off_local),
                        Rvalue::NullaryOp(NullOp::OffsetOf(offset_of_list), *adt_ty),
                    ))),
                );
                let total_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(total_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(head_off_local)),
                                Operand::Copy(Place::from(tail_bytes_local)),
                            )),
                        ),
                    ))),
                );

                (
                    Operand::Copy(Place::from(total_local)),
                    vec![
                        meta_stmt,
                        elem_size_stmt,
                        tail_bytes_stmt,
                        head_off_stmt,
                        total_stmt,
                    ],
                )
            }
        }
    }

    /// Recognize std/alloc Box wrappers that return a raw pointer but take an ADT (Box<T>) as input.
    ///
    /// In optimized MIR, `Box::into_raw` appears as a direct call where the argument is an ADT,
    /// so we cannot use TagProp (arg0 is not a thin pointer local). We therefore synthesize a root
    /// raw-pointer tag for the returned pointer local.
    fn is_box_into_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::into_raw")
    }

    fn is_box_new_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.ends_with("::new")
    }

    fn is_box_ty<'tcx>(&self, tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            TyKind::Adt(adt, _) => {
                let def_path = tcx.def_path_str(adt.did());
                def_path.contains("::boxed::Box") || def_path.contains("boxed::Box")
            }
            _ => false,
        }
    }

    fn is_box_from_raw_wrapper(&self, def_path: &str) -> bool {
        (def_path.contains("::boxed::Box") || def_path.contains("boxed::Box"))
            && def_path.contains("::from_raw")
    }

    fn direct_callee<'tcx>(
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
    fn call_arg_pointer_source_local<'tcx>(
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

    fn call_arg_lineage_source_local<'tcx>(
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

    fn backtrack_same_typed_call_result_source_local<'tcx>(
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
    fn backtrack_single_pointer_arg_call_result_source_local<'tcx>(
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
    fn backtrack_global_pointer_arg_call_result_source_local<'tcx>(
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

    fn ptr_derive_source_arg_index(&self, def_path: &str) -> usize {
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

    fn ptr_derive_call_requires_strict_validation(&self, def_path: &str) -> bool {
        def_path.contains("::ptr::")
            && (def_path.ends_with("::add")
                || def_path.ends_with("::sub")
                || def_path.ends_with("::offset")
                || def_path.ends_with("::wrapping_add")
                || def_path.ends_with("::wrapping_sub")
                || def_path.ends_with("::wrapping_offset")
                || def_path.ends_with("::byte_add")
                || def_path.ends_with("::byte_sub")
                || def_path.ends_with("::wrapping_byte_add")
                || def_path.ends_with("::wrapping_byte_sub"))
    }

    fn local_is_temp_like<'tcx>(&self, body: &Body<'tcx>, local: Local) -> bool {
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

    fn local_has_explicit_storage<'tcx>(&self, body: &Body<'tcx>, local: Local) -> bool {
        body.basic_blocks.iter().any(|block_data| {
            block_data.statements.iter().any(|stmt| {
                matches!(
                    stmt.kind,
                    StatementKind::StorageLive(l) | StatementKind::StorageDead(l) if l == local
                )
            })
        })
    }

    fn push_ptr_derive_call<'tcx>(
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

    fn push_ptr_derive_parent_call<'tcx>(
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

    fn push_box_into_raw_call<'tcx>(
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

    fn warn_unknown_call_if_needed<'tcx>(
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

    fn push_memop_call_effects<'tcx>(
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
                    self.memop_size_bytes(tcx, body, ptr_local, count_op, term.source_info.span)
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
                    let size_op =
                        self.memop_size_bytes(tcx, body, dst, count_op, term.source_info.span);
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
                                size_op: self.memop_size_bytes(
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

    fn push_alloc_shim_effects<'tcx>(
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

    fn scan_call_terminator<'tcx>(
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
        let ret_take_enabled = self.ret_take_enabled();

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

                CallEffect::CarrierCopyArg0 => {
                    if let (Some(dst_local), Some(tgt_bb)) =
                        (destination.as_local(), call_target_bb)
                    {
                        let dst_ty = body.local_decls[dst_local].ty;
                        if !self.is_pointer_ty(dst_ty) {
                            if let Some(src_place) = args
                                .get(0)
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
                                    structural_transport::pair_return_leafs_from_arg0(
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
                            let prefer_return_boundary_for_ref = callee_instrumented
                                && ret_take_enabled
                                && matches!(dst_ty.kind(), TyKind::Ref(..));
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
                                        structural_transport::pair_return_leafs_from_arg0(
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
                                            callee_path_opt.as_deref().is_some_and(|p| {
                                                self.ptr_derive_call_requires_strict_validation(p)
                                            }),
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
                                        callee_path_opt.as_deref().is_some_and(|p| {
                                            self.ptr_derive_call_requires_strict_validation(p)
                                        }),
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

        // Treat any pointer argument as tag relevant.
        for (arg_index, a) in args.iter().enumerate() {
            let Some(p) = self.place_from_operand(&a.node) else {
                continue;
            };
            let ty = p.ty(&body.local_decls, tcx).ty;
            if callee_instrumented
                && !self.is_pointer_ty(ty)
                && !p.projection.is_empty()
                && self.is_pointer_ty(body.local_decls[p.local].ty)
            {
                if let Some(callee_id) = callee_id_opt {
                    let exact_inplace_source =
                        p.projection.len() == 1 && matches!(p.projection[0], ProjectionElem::Deref);
                    let suppress_protector = !self.tb_call_arg_protector_supported_for_ty(
                        tcx,
                        body,
                        body.local_decls[p.local].ty,
                    );
                    let flags = self.call_arg_push_flags(exact_inplace_source, suppress_protector);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind: InstrKind::CallArgPush {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                            parent_mode: ParentSelectionMode::PointeeFamily,
                            flags,
                        },
                    });
                }
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
            if callee_instrumented {
                if let Some(callee_id) = callee_id_opt {
                    ptr_locals_needing_tag.insert(p.local);
                    let suppress_protector =
                        !self.tb_call_arg_protector_supported_for_ty(tcx, body, ty);
                    let flags = self.call_arg_push_flags(false, suppress_protector);
                    insert_points.push(InsertPoint {
                        bb,
                        stmt_idx: block_data.statements.len(),
                        insert_before: false,
                        source_info: term.source_info,
                        place: p,
                        kind: InstrKind::CallArgPush {
                            callee_id,
                            arg_index: arg_index as u64,
                            ptr_local: p.local,
                            parent_mode: ParentSelectionMode::PointeeFamily,
                            flags,
                        },
                    });
                }
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

        if callee_instrumented {
            if let Some(callee_id) = callee_id_opt {
                for (arg_index, a) in args.iter().enumerate() {
                    let Some(p) = self.place_from_operand(&a.node) else {
                        continue;
                    };
                    let ty = p.ty(&body.local_decls, tcx).ty;
                    if self.is_pointer_ty(ty) {
                        continue;
                    }
                    if self.supports_call_boundary_whole_slot_anchor_local(tcx, body, p.local)
                        && self.is_whole_place_slot_family_source(tcx, body, p)
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: p,
                            kind: InstrKind::CallArgPush {
                                callee_id,
                                arg_index: arg_index as u64,
                                ptr_local: p.local,
                                parent_mode: ParentSelectionMode::SlotFamily,
                                flags: 0,
                            },
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
                    if self.supports_call_boundary_leaf_shadow_ty(tcx, body, ty) {
                        for leaf_spec in self.shadowable_leaf_ptr_specs_from_place(tcx, body, p, ty)
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx: block_data.statements.len(),
                                insert_before: false,
                                source_info: term.source_info,
                                place: leaf_spec.place,
                                kind: InstrKind::CallArgLeafPush {
                                    callee_id,
                                    arg_index: arg_index as u64,
                                    leaf_key: leaf_spec.transport_key(),
                                },
                            });
                        }
                    }
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
                            for dst_leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
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
                                        leaf_key: dst_leaf_spec.transport_key(),
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
        // Non-pointer carriers with embedded source-level refs use `RetAnchorPush` /
        // `RetAnchorTake` (or `RetAnchorRoot` for opaque callees) so the destination local keeps
        // a whole-slot borrow family even though the MIR return place itself is not pointer-typed.
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
            if self.supports_call_boundary_ret_tag_ty(tcx, body, dst_ty) {
                if matches!(call_effect_opt, Some(CallEffect::PtrDerive))
                    && local_ptr_derive_emitted
                {
                    // Already modeled by the local PtrDerive insertion above.
                } else if callee_instrumented {
                    if !ret_take_enabled {
                        let is_mut = match dst_ty.kind() {
                            TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                            _ => false,
                        };
                        let is_ref = matches!(dst_ty.kind(), TyKind::Ref(..));

                        ptr_locals_needing_tag.insert(dst_local);
                        tagged_ptr_locals.insert(dst_local);
                        if let Some(tgt_bb) = call_target_bb {
                            insert_points.push(InsertPoint {
                                bb: tgt_bb,
                                stmt_idx: 0,
                                insert_before: false,
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
                    } else if let Some(callee_id) = callee_id_opt {
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
                && self.supports_call_boundary_leaf_shadow_ty(tcx, body, dst_ty)
            {
                if let Some(callee_id) = callee_id_opt {
                    // Raw-owner aggregates such as `BytesMut` export pointer leaf shadow from the
                    // callee, but they do not qualify for the ref-carrier anchor path. Import the
                    // returned leaf provenance directly into the caller's destination slots.
                    for dst_leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: dst_leaf_spec.place,
                            kind: InstrKind::RetLeafTake {
                                callee_id,
                                leaf_key: dst_leaf_spec.transport_key(),
                            },
                        });
                    }
                }
            } else if !self.is_pointer_ty(dst_ty)
                && callee_instrumented
                && self.supports_call_boundary_anchor_local(tcx, body, dst_local)
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
                    for dst_leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                        tcx,
                        body,
                        Place::from(dst_local),
                        dst_ty,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx: block_data.statements.len(),
                            insert_before: false,
                            source_info: term.source_info,
                            place: dst_leaf_spec.place,
                            kind: InstrKind::RetLeafTake {
                                callee_id,
                                leaf_key: dst_leaf_spec.transport_key(),
                            },
                        });
                    }
                }
            } else if !self.is_pointer_ty(dst_ty)
                && self.supports_call_boundary_anchor_local(tcx, body, dst_local)
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

    fn scan_drop_terminator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        bb: BasicBlock,
        block_data: &BasicBlockData<'tcx>,
        term: &Terminator<'tcx>,
        place: Place<'tcx>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
        ptr_locals_needing_tag: &mut HashSet<Local>,
        boundary_recovered_ptr_locals: &HashSet<Local>,
        tagged_ptr_locals: &mut HashSet<Local>,
        interesting_stack_locals: &HashSet<Local>,
    ) {
        let dropped_ty = place.ty(&body.local_decls, tcx).ty;
        let typing_env = body.typing_env(tcx);
        let dropped_ty = tcx
            .try_normalize_erasing_regions(typing_env, dropped_ty)
            .unwrap_or(dropped_ty);
        let drop_lang_item = tcx.require_lang_item(LangItem::DropInPlace, term.source_info.span);
        let drop_args = tcx.mk_args(&[dropped_ty.into()]);
        let mut callee_ids: Vec<u64> = Vec::new();
        if let Some(drop_def_id) = Instance::try_resolve(tcx, typing_env, drop_lang_item, drop_args)
            .ok()
            .flatten()
            .map(|instance| instance.def_id())
            .filter(|did| self.is_instrumented_callee(tcx, *did))
        {
            callee_ids.push(self.callee_id_u64(tcx, drop_def_id));
        }
        if let Some(dtor_def_id) = dropped_ty
            .ty_adt_def()
            .and_then(|adt| adt.destructor(tcx))
            .map(|dtor| dtor.did)
            .filter(|did| self.is_instrumented_callee(tcx, *did))
        {
            let callee_id = self.callee_id_u64(tcx, dtor_def_id);
            if !callee_ids.contains(&callee_id) {
                callee_ids.push(callee_id);
            }
        }
        if callee_ids.is_empty() {
            return;
        }

        if self.is_pointer_ty(dropped_ty) {
            ptr_locals_needing_tag.insert(place.local);
            tagged_ptr_locals.insert(place.local);
            let suppress_protector =
                !self.tb_call_arg_protector_supported_for_ty(tcx, body, dropped_ty);
            for callee_id in callee_ids {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place,
                    kind: InstrKind::CallArgPush {
                        callee_id,
                        arg_index: 0,
                        ptr_local: place.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        flags: self.call_arg_push_flags(false, suppress_protector),
                    },
                });
            }
            return;
        }

        if self.ty_contains_direct_ref_fields(tcx, dropped_ty) {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place,
                kind: InstrKind::CallArgValidate { local: place.local },
            });
        }

        if matches!(place.projection.first(), Some(ProjectionElem::Deref))
            && self.is_pointer_ty(body.local_decls[place.local].ty)
        {
            ptr_locals_needing_tag.insert(place.local);
            tagged_ptr_locals.insert(place.local);
            let suppress_protector = !self.tb_call_arg_protector_supported_for_ty(
                tcx,
                body,
                body.local_decls[place.local].ty,
            );
            for callee_id in callee_ids {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place,
                    kind: InstrKind::CallArgPush {
                        callee_id,
                        arg_index: 0,
                        ptr_local: place.local,
                        parent_mode: ParentSelectionMode::PointeeFamily,
                        flags: self.call_arg_push_flags(false, suppress_protector),
                    },
                });
            }
            return;
        }

        if !self.supports_call_boundary_anchor_local(tcx, body, place.local) {
            return;
        }

        for callee_id in callee_ids {
            insert_points.push(InsertPoint {
                bb,
                stmt_idx: block_data.statements.len(),
                insert_before: false,
                source_info: term.source_info,
                place,
                kind: InstrKind::CallArgPush {
                    callee_id,
                    arg_index: 0,
                    ptr_local: place.local,
                    parent_mode: ParentSelectionMode::SlotFamily,
                    flags: 0,
                },
            });
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
                    if self.ret_push_enabled()
                        && self.supports_call_boundary_ret_tag_ty(tcx, body, body.return_ty())
                    {
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
                    if self.supports_call_boundary_anchor_local(tcx, body, RETURN_PLACE) {
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
                    if self.ty_contains_direct_pointer_fields(tcx, body, body.return_ty()) {
                        let leaf_ptrs = self.shadowable_leaf_ptr_places_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            body.return_ty(),
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
                        for leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
                            tcx,
                            body,
                            Place::from(RETURN_PLACE),
                            body.return_ty(),
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
                        if !pointee_ty.is_sized(tcx, body.typing_env(tcx)) {
                            continue;
                        }
                        if !self.ty_contains_pointer_fields(tcx, body, pointee_ty, 4) {
                            continue;
                        }
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
                        for leaf_spec in self.shadowable_leaf_ptr_specs_from_place(
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

    fn allocate_tag_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        ptrs: HashSet<Local>,
    ) -> HashMap<Local, Local> {
        let mut tag_local_for_ptr_local: HashMap<Local, Local> = HashMap::new();
        for ptr_local in ptrs.into_iter() {
            if !tag_local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
                tag_local_for_ptr_local.insert(ptr_local, t);
            }
        }
        tag_local_for_ptr_local
    }

    fn allocate_u8_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        ptrs: HashSet<Local>,
    ) -> HashMap<Local, Local> {
        let mut local_for_ptr_local: HashMap<Local, Local> = HashMap::new();
        for ptr_local in ptrs.into_iter() {
            if !local_for_ptr_local.contains_key(&ptr_local) {
                let t = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u8, rustc_span::DUMMY_SP));
                local_for_ptr_local.insert(ptr_local, t);
            }
        }
        local_for_ptr_local
    }

    fn allocate_reborrow_anchor_locals<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        anchor_specs: &ReborrowAnchorSpecMap,
    ) -> HashMap<String, Local> {
        let mut anchor_local_for_key: HashMap<String, Local> = HashMap::new();
        let mut keys: Vec<&String> = anchor_specs.keys().collect();
        keys.sort();
        for key in keys {
            let local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.u64, rustc_span::DUMMY_SP));
            anchor_local_for_key.insert(key.clone(), local);
        }
        anchor_local_for_key
    }

    fn ptr_state_locals_for_ptr_local(
        &self,
        ptr_local: Local,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
    ) -> Option<PtrStateLocals> {
        Some(PtrStateLocals {
            tag_local: tag_local_for_ptr_local.get(&ptr_local).copied()?,
            ref_ancestor_local: ref_ancestor_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_parent_local: export_parent_local_for_ptr_local.get(&ptr_local).copied(),
            boundary_recovered_local: export_parent_is_recovered_local_for_ptr_local
                .get(&ptr_local)
                .copied(),
        })
    }

    fn carrier_slot_locals_for_local(
        &self,
        local: Local,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
    ) -> Option<CarrierSlotLocals> {
        Some(CarrierSlotLocals {
            anchor_local: reborrow_anchor_local_for_stack_local.get(&local).copied()?,
            slot_family_valid_local: anchor_is_slot_family_local_for_stack_local
                .get(&local)
                .copied(),
        })
    }

    fn schedule_reborrow_anchor_resets<'tcx>(
        &self,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        reborrow_anchor_specs: &ReborrowAnchorSpecMap,
        reborrow_anchor_local_for_key: &HashMap<String, Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let source_info = stmt.source_info;
                let touched_local = match stmt.kind {
                    StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                        Some(local)
                    }
                    StatementKind::Assign(box (place, _)) => place.as_local(),
                    _ => None,
                };
                if let Some(local) = touched_local {
                    if let Some(slot_state) = self.carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    ) {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info,
                            place: Place::from(slot_state.anchor_local),
                            kind: InstrKind::ReborrowAnchorZero {
                                anchor_local: slot_state.anchor_local,
                                anchor_state_local: slot_state.slot_family_valid_local,
                            },
                        });
                    }
                    for (key, deps) in reborrow_anchor_specs.iter() {
                        if deps.contains(&local) {
                            if let Some(anchor_local) =
                                reborrow_anchor_local_for_key.get(key).copied()
                            {
                                insert_points.push(InsertPoint {
                                    bb,
                                    stmt_idx,
                                    insert_before: false,
                                    source_info,
                                    place: Place::from(anchor_local),
                                    kind: InstrKind::ReborrowAnchorZero {
                                        anchor_local,
                                        anchor_state_local: None,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    fn schedule_reborrow_anchor_propagation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        boundary_recovered_ptr_locals: &HashSet<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let Some(dst_slot_state) = self.carrier_slot_locals_for_local(
                    dst_local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };
                if let Rvalue::Aggregate(_, operands) = rvalue {
                    let mut unique_src_local: Option<Local> = None;
                    let mut ambiguous = false;
                    for operand in operands.iter() {
                        let Some(src_place) = self.place_from_operand(operand) else {
                            continue;
                        };
                        if !src_place.projection.is_empty() {
                            continue;
                        }
                        let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                        if !self.is_pointer_ty(src_ty)
                            && !reborrow_anchor_local_for_stack_local.contains_key(&src_place.local)
                        {
                            continue;
                        }
                        match unique_src_local {
                            Some(existing) if existing != src_place.local => {
                                ambiguous = true;
                                break;
                            }
                            Some(_) => {}
                            None => unique_src_local = Some(src_place.local),
                        }
                    }
                    if let (Some(src_local), false) = (unique_src_local, ambiguous) {
                        if src_local != dst_local {
                            let src_ty = body.local_decls[src_local].ty;
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local,
                                    mark_slot_family: self
                                        .supports_call_boundary_anchor_local(tcx, body, dst_local)
                                        && boundary_recovered_ptr_locals.contains(&src_local)
                                        && matches!(
                                            src_ty.kind(),
                                            TyKind::Ref(_, _, Mutability::Not)
                                        ),
                                },
                            });
                            continue;
                        }
                    }
                }
                let src_place = match rvalue {
                    Rvalue::Use(Operand::Copy(src_place))
                    | Rvalue::Use(Operand::Move(src_place)) => *src_place,
                    _ => continue,
                };
                let src_local = src_place.local;
                if src_local == dst_local {
                    continue;
                }
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty)
                    && !src_place.projection.is_empty()
                    && self.supports_slot_family_local(tcx, body, dst_local)
                {
                    if let Some(recovered_src_local) = self
                        .backtrack_same_typed_call_result_source_local(tcx, body, src_local, dst_ty)
                    {
                        if recovered_src_local != src_local {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                            continue;
                        }
                    }
                }
                if !reborrow_anchor_local_for_stack_local.contains_key(&src_local) {
                    let src_ty = src_place.ty(&body.local_decls, tcx).ty;
                    if !self.is_pointer_ty(dst_ty)
                        && !src_place.projection.is_empty()
                        && self.supports_slot_family_local(tcx, body, dst_local)
                    {
                        if let Some(recovered_src_local) = self
                            .backtrack_same_typed_call_result_source_local(
                                tcx, body, src_local, dst_ty,
                            )
                        {
                            insert_points.push(InsertPoint {
                                bb,
                                stmt_idx,
                                insert_before: false,
                                source_info: stmt.source_info,
                                place: Place::from(dst_local),
                                kind: InstrKind::ReborrowAnchorSeed {
                                    dst_local,
                                    src_local: recovered_src_local,
                                    mark_slot_family: true,
                                },
                            });
                            continue;
                        }
                    }
                    if src_ty == dst_ty
                        && self.is_pointer_ty(body.local_decls[src_local].ty)
                        && matches!(src_place.projection.first(), Some(ProjectionElem::Deref))
                    {
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: Place::from(dst_local),
                            kind: InstrKind::ReborrowAnchorSet {
                                dst_local,
                                anchor_local: dst_slot_state.anchor_local,
                                anchor_state_local: dst_slot_state.slot_family_valid_local,
                                src_ptr_local: src_local,
                            },
                        });
                    }
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: Place::from(dst_local),
                    kind: InstrKind::ReborrowAnchorSeed {
                        dst_local,
                        src_local,
                        mark_slot_family: !src_place.projection.is_empty()
                            && self.supports_call_boundary_anchor_local(tcx, body, dst_local),
                    },
                });
            }
        }
    }

    fn init_tag_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        skip_ptr_locals: &HashSet<Local>,
    ) {
        // Initialize tag locals at function entry so we never read uninitialized tag values
        // (which would show up as `unknown tag=<garbage>` in the runtime).
        //
        // This does NOT solve inter-procedural tag passing/retagging by itself; it just ensures
        // the default is `0` ("untagged") rather than uninitialized memory.
        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for (ptr_local, tag_local) in tag_local_for_ptr_local.iter() {
            // Argument tags are set by ArgRetag at entry; avoid overwriting them with zero.
            if skip_ptr_locals.contains(ptr_local) {
                init_stmts.push(Statement::new(
                    source_info,
                    StatementKind::StorageLive(*tag_local),
                ));
                continue;
            }
            // Ensure the tag local is live, then initialize it to 0 ("untagged").
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));

            let zero: Operand<'tcx> = self.const_u64(tcx, source_info.span, 0);
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((Place::from(*tag_local), Rvalue::Use(zero)))),
            ));
        }

        // Insert right after the initial StorageLive prologue in the entry block.
        // This avoids reordering rustc's own prologue statements and ensures our locals
        // are considered live before we assign to them.
        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[entry_bb];

        let mut insert_at = 0usize;
        while insert_at < bd.statements.len() {
            match bd.statements[insert_at].kind {
                StatementKind::StorageLive(_) => insert_at += 1,
                _ => break,
            }
        }

        bd.statements.splice(insert_at..insert_at, init_stmts);
    }

    fn init_extra_tag_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        tag_locals: &[Local],
    ) {
        if tag_locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for tag_local in tag_locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*tag_local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*tag_local),
                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                ))),
            ));
        }

        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[entry_bb];
        let mut insert_at = 0usize;
        while insert_at < bd.statements.len() {
            match bd.statements[insert_at].kind {
                StatementKind::StorageLive(_) => insert_at += 1,
                _ => break,
            }
        }
        bd.statements.splice(insert_at..insert_at, init_stmts);
    }

    fn init_extra_u8_locals_to_zero<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        locals: &[Local],
    ) {
        if locals.is_empty() {
            return;
        }

        let entry_bb = START_BLOCK;
        let source_info = SourceInfo {
            span: rustc_span::DUMMY_SP,
            scope: OUTERMOST_SOURCE_SCOPE,
        };

        let mut init_stmts: Vec<Statement<'tcx>> = Vec::new();
        for local in locals {
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::StorageLive(*local),
            ));
            init_stmts.push(Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(*local),
                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                ))),
            ));
        }

        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[entry_bb];
        let mut insert_at = 0usize;
        while insert_at < bd.statements.len() {
            match bd.statements[insert_at].kind {
                StatementKind::StorageLive(_) => insert_at += 1,
                _ => break,
            }
        }
        bd.statements.splice(insert_at..insert_at, init_stmts);
    }

    /// Add holder acquire/release hooks around hidden tag-local writes after the main MIR
    /// instrumentation pass has materialized them.
    fn schedule_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        manual_holder_managed_tag_assignments: &HashSet<(BasicBlock, usize)>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        let mut ptr_local_for_tag_local: HashMap<Local, Local> = HashMap::new();
        for (ptr_local, tag_local) in tag_local_for_ptr_local {
            ptr_local_for_tag_local.insert(*tag_local, *ptr_local);
        }

        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                match stmt.kind {
                    StatementKind::Assign(box (dst_place, _)) => {
                        if manual_holder_managed_tag_assignments.contains(&(bb, stmt_idx)) {
                            continue;
                        }
                        let Some(dst_local) = dst_place.as_local() else {
                            continue;
                        };
                        let Some(ptr_local) = ptr_local_for_tag_local.get(&dst_local).copied()
                        else {
                            continue;
                        };
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagLocalKill {
                                tag_local: dst_local,
                            },
                        });
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: false,
                            source_info: stmt.source_info,
                            place: dst_place,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                    StatementKind::StorageDead(dead_local) => {
                        if !tag_local_for_ptr_local.contains_key(&dead_local) {
                            continue;
                        }
                        insert_points.push(InsertPoint {
                            bb,
                            stmt_idx,
                            insert_before: true,
                            source_info: stmt.source_info,
                            place: Place::from(dead_local),
                            kind: InstrKind::TagKill {
                                ptr_local: dead_local,
                            },
                        });
                    }
                    _ => {}
                }
            }

            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            let TerminatorKind::Call {
                destination,
                target,
                ..
            } = &term.kind
            else {
                continue;
            };
            if let Some(target_bb) = *target {
                if let Some(dst_local) = destination.as_local() {
                    if ptr_local_for_tag_local.contains_key(&dst_local) {
                        insert_points.push(InsertPoint {
                            bb: target_bb,
                            stmt_idx: 0,
                            insert_before: false,
                            source_info: term.source_info,
                            place: *destination,
                            kind: InstrKind::TagRetain {
                                tag_local: dst_local,
                            },
                        });
                    }
                }
            }
        }
    }

    fn schedule_aux_tag_local_holder_updates<'tcx>(
        &self,
        body: &Body<'tcx>,
        aux_tag_locals: &HashSet<Local>,
        insert_points: &mut Vec<InsertPoint<'tcx>>,
    ) {
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                let StatementKind::Assign(box (dst_place, _)) = stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                if !aux_tag_locals.contains(&dst_local) {
                    continue;
                }
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: true,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagLocalKill {
                        tag_local: dst_local,
                    },
                });
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx,
                    insert_before: false,
                    source_info: stmt.source_info,
                    place: dst_place,
                    kind: InstrKind::TagRetain {
                        tag_local: dst_local,
                    },
                });
            }
        }
    }

    fn insert_instrumentation<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        insert_points: Vec<InsertPoint<'tcx>>,
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        fn resolve_return_chain_bb<'tcx>(body: &Body<'tcx>, mut bb: BasicBlock) -> BasicBlock {
            for _ in 0..64 {
                let Some(term) = body.basic_blocks[bb].terminator.as_ref() else {
                    break;
                };
                match &term.kind {
                    TerminatorKind::Return => break,
                    // End-of-block instrumentation on a return site can be applied after the
                    // block has already been split into a synthetic call chain. Follow the
                    // continuation so return-side hooks still land immediately before the
                    // real `Return`.
                    TerminatorKind::Call {
                        target: Some(next), ..
                    } if body.basic_blocks[bb].statements.is_empty() => {
                        bb = *next;
                    }
                    _ => break,
                }
            }
            bb
        }

        fn is_runtime_hook_call<'tcx>(
            tcx: TyCtxt<'tcx>,
            term: &Terminator<'tcx>,
        ) -> Option<BasicBlock> {
            let TerminatorKind::Call {
                func,
                target: Some(next),
                ..
            } = &term.kind
            else {
                return None;
            };

            let Operand::Constant(c) = func else {
                return None;
            };
            let TyKind::FnDef(def_id, _) = c.const_.ty().kind() else {
                return None;
            };
            (tcx.crate_name(def_id.krate).as_str() == "runtime").then_some(*next)
        }

        fn resolve_split_chain_insert_site<'tcx>(
            tcx: TyCtxt<'tcx>,
            body: &Body<'tcx>,
            orig_stmt_prefix_len: &HashMap<BasicBlock, usize>,
            mut bb: BasicBlock,
            mut stmt_idx: usize,
            insert_before: bool,
        ) -> (BasicBlock, usize) {
            for _ in 0..64 {
                let bd = &body.basic_blocks[bb];
                let len = orig_stmt_prefix_len
                    .get(&bb)
                    .copied()
                    .unwrap_or_else(|| bd.statements.len());
                let needs_follow = if insert_before {
                    stmt_idx > len
                } else {
                    stmt_idx >= len
                };
                if !needs_follow {
                    break;
                }
                let Some(term) = bd.terminator.as_ref() else {
                    break;
                };
                let Some(next) = is_runtime_hook_call(tcx, term) else {
                    break;
                };
                stmt_idx = stmt_idx.saturating_sub(len);
                bb = next;
            }
            (bb, stmt_idx)
        }

        fn instr_priority(kind: &InstrKind<'_>) -> u8 {
            match kind {
                InstrKind::Ref { .. }
                | InstrKind::Raw { .. }
                | InstrKind::RawRoot { .. }
                | InstrKind::FnEnter { .. }
                | InstrKind::ArgRetag { .. }
                | InstrKind::ArgAnchorTake { .. }
                | InstrKind::ArgLeafTake { .. }
                | InstrKind::RetRoot { .. }
                | InstrKind::PtrDerive { .. }
                | InstrKind::PtrDeriveParent { .. } => 0,
                // Tag propagation must execute after tag-creating hooks but before
                // access/usage hooks at the same insertion site.
                InstrKind::TagProp { .. }
                | InstrKind::TagPropFromRefAncestor { .. }
                | InstrKind::FnExit { .. }
                | InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. }
                | InstrKind::ReborrowAnchorSet { .. }
                | InstrKind::ReborrowAnchorSeed { .. }
                | InstrKind::ReborrowAnchorZero { .. }
                | InstrKind::ParentTagSnapshot { .. }
                | InstrKind::ArgAnchorSeedFromShadow { .. } => 1,
                InstrKind::ShadowLoad { .. } => 1,
                // Debug ref activation may need the restored tag/ref_ancestor emitted by
                // ShadowLoad or TagProp at the same definition site.
                InstrKind::DebugRefActivate { .. } => 2,
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. }
                // Return-boundary exports must run before FnExit tears down the callee frame.
                | InstrKind::RetAnchorPush { .. }
                | InstrKind::RetPush { .. }
                | InstrKind::RetLeafPush { .. }
                | InstrKind::RetAnchorRoot { .. }
                | InstrKind::MutArgRetPush { .. }
                | InstrKind::MutArgRetLeafPush { .. }
                | InstrKind::ShadowKill { .. } => 2,
                // Keep stack-slot writes in the same bucket as reborrow-anchor maintenance so
                // the later-scheduled anchor zeroing is inserted first and then moved after the
                // write call when we split the block at this statement.
                InstrKind::StackSlotWriteAllowUntagged { .. } => 1,
                InstrKind::ShadowStore { .. }
                | InstrKind::ShadowStoreBoxPointee { .. }
                | InstrKind::ShadowCopySlot { .. }
                | InstrKind::ShadowCopyRange { .. } => 3,
                InstrKind::CallArgPush { .. }
                | InstrKind::CallArgValidate { .. }
                | InstrKind::CallArgLeafPush { .. }
                | InstrKind::PtrUse { .. }
                | InstrKind::RetValidate { .. }
                | InstrKind::RetAnchorTake { .. }
                | InstrKind::RetLeafTake { .. }
                | InstrKind::MutArgRetTake { .. }
                | InstrKind::MutArgRetLeafTake { .. }
                | InstrKind::MutArgRetTakePtrOnly { .. } => 3,
                _ => 4,
            }
        }

        // Entry-side arg retag/anchor initialization must run before any ptr reads/writes.
        // We split it out so we can enforce ordering independent of stmt_idx sorting.
        let mut arg_retag_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();
        let mut other_points: Vec<(usize, InsertPoint<'tcx>)> = Vec::new();

        for (idx, ip) in insert_points.into_iter().enumerate() {
            if matches!(
                ip.kind,
                InstrKind::FnEnter { .. }
                    | InstrKind::ArgRetag { .. }
                    | InstrKind::ArgAnchorTake { .. }
                    | InstrKind::ArgAnchorSeedFromShadow { .. }
                    | InstrKind::ArgLeafTake { .. }
            ) {
                arg_retag_points.push((idx, ip));
            } else {
                other_points.push((idx, ip));
            }
        }

        let sort_points = |points: &mut Vec<(usize, InsertPoint<'tcx>)>| {
            points.sort_by_key(|(idx, ip)| {
                (ip.bb.index(), ip.stmt_idx, instr_priority(&ip.kind), *idx)
            });
        };

        sort_points(&mut other_points);
        sort_points(&mut arg_retag_points);

        let mut orig_stmt_prefix_len: HashMap<BasicBlock, usize> = HashMap::new();

        for (_idx, ip) in other_points.into_iter().rev() {
            let mut bb = ip.bb;
            let mut stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            let (resolved_bb, resolved_stmt_idx) = resolve_split_chain_insert_site(
                tcx,
                body,
                &orig_stmt_prefix_len,
                bb,
                stmt_idx,
                ip.insert_before,
            );
            bb = resolved_bb;
            stmt_idx = resolved_stmt_idx;

            if matches!(creation_kind, InstrKind::StackAlloc { live: false, .. })
                && !ip.insert_before
            {
                let resolved_bb = resolve_return_chain_bb(body, bb);
                if resolved_bb != bb {
                    bb = resolved_bb;
                    stmt_idx = body.basic_blocks[bb].statements.len();
                }
            }

            // Avoid emitting allow-untagged READ/WRITE for raw-pointer args from unknown calls.
            // These are a common source of false positives (e.g., pointer casts).
            if let InstrKind::PtrReadAllowUntagged { ptr_local, .. }
            | InstrKind::PtrWriteAllowUntagged { ptr_local, .. } = creation_kind
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if matches!(ptr_ty.kind(), TyKind::RawPtr(..)) {
                    continue;
                }
            }

            if let InstrKind::ShadowLoad {
                dst_local,
                require_tag,
                validate_ref,
            } = creation_kind.clone()
            {
                let Some(dst_ptr_state) = self.ptr_state_locals_for_ptr_local(
                    dst_local,
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    export_parent_local_for_ptr_local,
                    export_parent_is_recovered_local_for_ptr_local,
                ) else {
                    continue;
                };
                let Some(dst_ref_ancestor_local) = dst_ptr_state.ref_ancestor_local else {
                    continue;
                };
                let dst_export_parent_local = dst_ptr_state.boundary_parent_local;
                let dst_recovered_local = dst_ptr_state.boundary_recovered_local;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let ptr_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((ptr_addr_stmt1_opt, ptr_addr_stmt2)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(dst_local),
                    ptr_addr_local,
                ) else {
                    continue;
                };

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let ref_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                let dst_ty = body.local_decls[dst_local].ty;
                let needs_loaded_ref_validation =
                    matches!(dst_ty.kind(), TyKind::Ref(..)) && (require_tag || validate_ref);
                let projected_field_ty = place.ty(&body.local_decls, tcx).ty;
                let projected_carrier_anchor = if !place.projection.is_empty()
                    && matches!(projected_field_ty.kind(), TyKind::Ref(..))
                    && !self.is_pointer_ty(body.local_decls[place.local].ty)
                {
                    reborrow_anchor_local_for_stack_local
                        .get(&place.local)
                        .copied()
                } else {
                    None
                };
                let post_load_block = if projected_carrier_anchor.is_some() {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else if needs_loaded_ref_validation {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                } else {
                    cont_block
                };
                let validate_block = if needs_loaded_ref_validation {
                    if projected_carrier_anchor.is_some() {
                        Some(
                            body.basic_blocks_mut()
                                .push(BasicBlockData::new(None, is_cleanup)),
                        )
                    } else {
                        Some(post_load_block)
                    }
                } else {
                    None
                };

                let tag_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_tag_for_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let tag_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ref_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_load_ref_ancestor_for_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let export_parent_func = dst_export_parent_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent_for_ptr,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let export_parent_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let recovered_func = dst_recovered_local.map(|_| {
                    Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_load_export_parent_recovered_for_ptr,
                        std::iter::empty(),
                        source_info.span,
                    )
                });
                let recovered_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(ptr_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let final_post_load_block = post_load_block;
                let recovered_block = dst_recovered_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let export_parent_target = recovered_block.unwrap_or(final_post_load_block);
                let export_parent_block = dst_export_parent_local.map(|_| {
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup))
                });
                let ref_target = export_parent_block.unwrap_or(export_parent_target);

                body.basic_blocks_mut()[ref_block].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: ref_func,
                        args: ref_args,
                        destination: Place::from(dst_ref_ancestor_local),
                        target: Some(ref_target),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });

                if let (
                    Some(export_parent_block),
                    Some(export_parent_local),
                    Some(export_parent_func),
                ) = (
                    export_parent_block,
                    dst_export_parent_local,
                    export_parent_func,
                ) {
                    body.basic_blocks_mut()[export_parent_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: export_parent_func,
                            args: export_parent_args,
                            destination: Place::from(export_parent_local),
                            target: Some(export_parent_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let (Some(recovered_block), Some(recovered_local), Some(recovered_func)) =
                    (recovered_block, dst_recovered_local, recovered_func)
                {
                    body.basic_blocks_mut()[recovered_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: recovered_func,
                            args: recovered_args,
                            destination: Place::from(recovered_local),
                            target: Some(final_post_load_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                if let Some(anchor_local) = projected_carrier_anchor {
                    let tag_is_zero_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                    let validate_target = validate_block.unwrap_or(cont_block);
                    let repair_cont_block = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: validate_target,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[repair_cont_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_local))),
                            ))),
                        ));
                    if let Some(dst_export_parent_local) = dst_export_parent_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        dst_ptr_state.tag_local,
                                    ))),
                                ))),
                            ));
                    }
                    if let Some(dst_recovered_local) = dst_recovered_local {
                        body.basic_blocks_mut()[repair_cont_block]
                            .statements
                            .push(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                                ))),
                            ));
                    }

                    let repair_call_block = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(None, is_cleanup));
                    let repair_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (repair_addr_stmt1_opt, repair_addr_stmt2) = self
                        .addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(dst_local),
                            repair_addr_local,
                        )
                        .expect("ShadowLoad projected ref repair on non-pointer local");
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    let dst_ty = body.local_decls[dst_local].ty;
                    let is_mut_u8 = match dst_ty.kind() {
                        TyKind::Ref(_, _, Mutability::Mut) => 1,
                        _ => 0,
                    };
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    // Missing field-slot shadow on projected carrier refs should be repaired
                    // by rebuilding a ref tag from the loaded pointer value, not by copying the
                    // outer carrier anchor into the local tag directly.
                    alias_flags |= 0b10;
                    let repair_ref_func = Operand::function_handle(
                        tcx,
                        hooks.def_id_ref,
                        std::iter::empty(),
                        source_info.span,
                    );
                    let repair_ref_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(repair_addr_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, is_mut_u8),
                            span: source_info.span,
                        },
                        Spanned {
                            node: Operand::Copy(Place::from(anchor_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, alias_flags),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    let repair_bd = &mut body.basic_blocks_mut()[repair_call_block];
                    if let Some(stmt) = repair_addr_stmt1_opt {
                        repair_bd.statements.push(stmt);
                    }
                    repair_bd.statements.push(repair_addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        repair_bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        repair_bd.statements.append(&mut align_stmts);
                    }
                    repair_bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: repair_ref_func,
                            args: repair_ref_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(repair_cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });

                    body.basic_blocks_mut()[post_load_block]
                        .statements
                        .push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tag_is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ));
                    body.basic_blocks_mut()[post_load_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::SwitchInt {
                            discr: Operand::Copy(Place::from(tag_is_zero_local)),
                            targets: SwitchTargets::static_if(
                                0,
                                validate_target,
                                repair_call_block,
                            ),
                        },
                    });
                }

                if let Some(validate_block) = validate_block {
                    let validate_func = Operand::function_handle(
                        tcx,
                        if require_tag {
                            hooks.def_id_require_loaded_ptr_tag
                        } else {
                            hooks.def_id_validate_loaded_ref_tag
                        },
                        std::iter::empty(),
                        source_info.span,
                    );
                    let validate_args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(dst_ptr_state.tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    body.basic_blocks_mut()[validate_block].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: validate_func,
                            args: validate_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                }

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(slot_addr_stmt1);
                    bd.statements.push(slot_addr_stmt2);
                    if let Some(ptr_addr_stmt1) = ptr_addr_stmt1_opt {
                        bd.statements.push(ptr_addr_stmt1);
                    }
                    bd.statements.push(ptr_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: tag_func,
                            args: tag_args,
                            destination: Place::from(dst_ptr_state.tag_local),
                            target: Some(ref_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopySlot { src_place } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_slot,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowCopyRange { src_place, size_op } = creation_kind.clone() {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let src_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    dst_addr_local,
                    true,
                ) else {
                    continue;
                };
                let Some((src_addr_stmt1, src_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    src_place,
                    src_addr_local,
                    false,
                ) else {
                    continue;
                };

                let (arg_size, mut size_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &size_op);
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let copy_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_copy_range,
                    std::iter::empty(),
                    source_info.span,
                );
                let copy_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(src_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_size,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.statements.push(src_addr_stmt1);
                    bd.statements.push(src_addr_stmt2);
                    bd.statements.append(&mut size_stmts);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: copy_func,
                            args: copy_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ShadowStoreBoxPointee {
                box_local,
                src_local,
            } = creation_kind.clone()
            {
                let dst_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((dst_addr_stmt1, dst_addr_stmt2)) = self
                    .box_pointee_slot_addr_stmts_for_local(
                        tcx,
                        body,
                        source_info,
                        box_local,
                        dst_addr_local,
                    )
                else {
                    continue;
                };

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let tag_op: Operand<'tcx> =
                    if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let ref_ancestor_op: Operand<'tcx> =
                    if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                let export_parent_op: Operand<'tcx> =
                    if let Some(tl) = export_parent_local_for_ptr_local.get(&src_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        tag_op.clone()
                    };
                let recovered_op: Operand<'tcx> = if let Some(tl) =
                    export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                {
                    Operand::Copy(Place::from(*tl))
                } else {
                    self.const_u8(tcx, source_info.span, 0)
                };
                let store_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_shadow_store_ptr,
                    std::iter::empty(),
                    source_info.span,
                );
                let store_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(dst_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: ref_ancestor_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: export_parent_op,
                        span: source_info.span,
                    },
                    Spanned {
                        node: recovered_op,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(dst_addr_stmt1);
                    bd.statements.push(dst_addr_stmt2);
                    bd.terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: store_func,
                            args: store_args,
                            destination: Place::from(tmp_unit),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    });
                    rem
                };

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            // workaround for pointers produced from NonNull/Unique via Transmute
            // RawRoot lowering: we implement this by mirroring the existing Raw lowering code path:
            //   tag(ptr_local) = __record_raw_ptr_creation(expose(ptr_local), is_mut, 0)
            if let InstrKind::RawRoot {
                ptr_local,
                is_mut,
                exposed_provenance,
            } = creation_kind.clone()
            {
                let ptr_ty = body.local_decls[ptr_local].ty;
                if !self.is_pointer_ty(ptr_ty) {
                    continue;
                }
                let raw_root_is_ref = matches!(ptr_ty.kind(), TyKind::Ref(..));
                let dst_tag = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RawRoot");

                // Compute exposed address first. This is fallible for some pointer shapes, and we must
                // not mutate CFG until we know RawRoot lowering can be emitted completely.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let Some((data_ptr_stmt_opt, addr_stmt)) = self.addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    Place::from(ptr_local),
                    addr_local,
                ) else {
                    continue;
                };
                let is_mut_u8: u8 = if is_mut { 1 } else { 0 };
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
                if !bounds_len_stmts.is_empty() {
                    // Appended after address statements once call_bb exists.
                }

                // We insert using the same “split block with a call terminator” style used elsewhere.
                // Create fresh block that will run the call and then continue.
                let is_cleanup = body.basic_blocks[bb].is_cleanup;

                // Split the current block at the correct position so RawRoot runs
                // before or after the target statement based on insert_before.
                let mut tail_stmts: Vec<Statement<'tcx>> = Vec::new();
                {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let len = bd.statements.len();
                    let split_at = if stmt_idx >= len {
                        len
                    } else if ip.insert_before {
                        stmt_idx
                    } else {
                        stmt_idx + 1
                    };
                    tail_stmts.extend(bd.statements.drain(split_at..));
                }

                let orig_term = body.basic_blocks[bb].terminator.clone();
                let cont_bb = {
                    let mut cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    cont_data.statements = tail_stmts;
                    body.basic_blocks_mut().push(cont_data)
                };

                // Rewrite original terminator to jump to the new RawRoot call block.
                // Create the RawRoot call block and set it as the new successor.
                let call_bb = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto { target: call_bb },
                });

                if let Some(data_ptr_stmt) = data_ptr_stmt_opt {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .push(data_ptr_stmt);
                }
                body.basic_blocks_mut()[call_bb].statements.push(addr_stmt);
                if !bounds_len_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut bounds_len_stmts);
                }
                if !align_stmts.is_empty() {
                    body.basic_blocks_mut()[call_bb]
                        .statements
                        .append(&mut align_stmts);
                }

                let root_func = Operand::function_handle(
                    tcx,
                    if raw_root_is_ref {
                        hooks.def_id_ref
                    } else {
                        hooks.def_id_raw
                    },
                    std::iter::empty(),
                    source_info.span,
                );
                let args_root: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            (if alias_exempt { 1 } else { 0 })
                                | (if exposed_provenance { 0b10_0000 } else { 0 }),
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[call_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: root_func,
                        args: args_root,
                        destination: Place::from(dst_tag),
                        target: Some(cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Normal,
                        fn_span: source_info.span,
                    },
                });

                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    body.basic_blocks_mut()[cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(if raw_root_is_ref {
                                    Operand::Copy(Place::from(dst_tag))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }),
                            ))),
                        ),
                    );
                }

                // Done handling this insert point.
                continue;
            }

            // Caller-side: take return tag after a call returned a pointer into `dst_local`.
            //
            // IMPORTANT: this must be per-call-edge, not per-target-block.
            // A single target block may have multiple call predecessors. If we patch the
            // target block in-place, we would incorrectly run one call's RetTake logic for
            // all predecessors and corrupt tag lineage.
            //
            // We therefore create:
            //   call_bb -> ret_take_bb -> ret_take_cont_bb -> orig_target
            // and only rewrite this call's `target` to `ret_take_bb`.
            if let InstrKind::MutArgRetTake {
                callee_id,
                arg_index,
                local,
                ptr_local,
            } = creation_kind
            {
                let Some(slot_state) = self.carrier_slot_locals_for_local(
                    local,
                    reborrow_anchor_local_for_stack_local,
                    anchor_is_slot_family_local_for_stack_local,
                ) else {
                    continue;
                };

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("MutArgRetTake on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTake expected a Call terminator"),
                    }
                }

                body.basic_blocks_mut()[ret_take_cont_bb].statements.splice(
                    0..0,
                    [
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Eq,
                                    Box::new((
                                        Operand::Copy(Place::from(dst_tag_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(is_zero_u64_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(is_zero_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(nonzero_u64_local),
                                Rvalue::BinaryOp(
                                    BinOp::Sub,
                                    Box::new((
                                        self.const_u64(tcx, source_info.span, 1),
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(keep_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(is_zero_u64_local)),
                                        Operand::Copy(Place::from(slot_state.anchor_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(new_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(nonzero_u64_local)),
                                        Operand::Copy(Place::from(dst_tag_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(selected_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(keep_part_local)),
                                        Operand::Copy(Place::from(new_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(slot_state.anchor_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ),
                    ],
                );
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        7,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            if let InstrKind::RetTake {
                callee_id,
                dst_local,
            } = creation_kind
            {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing tag local for RetTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                // Compute the address from the return place. For wide pointers we extract the
                // data pointer first so the tag maps to the same address used by raw reads.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(dst_local),
                        addr_local,
                    )
                    .expect("RetTake on non-pointer local");

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_ret_tag_or_root,
                    std::iter::empty(),
                    source_info.span,
                );

                let dst_ty = body.local_decls[dst_local].ty;
                let is_mut = match dst_ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    TyKind::RawPtr(_ty, mutbl) => matches!(mutbl, Mutability::Mut),
                    _ => false,
                };
                let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                        // Call-return ref retagging can lose the caller-side parent tag and
                        // materialize a fresh root at the same stack address. Mark these for
                        // runtime same-address lineage repair.
                        flags |= 0b10;
                    }
                    flags
                };
                let bounds_len_op = if matches!(dst_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        dst_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, dst_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, dst_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 }),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        take_bd.statements.push(addr_stmt1);
                    }
                    take_bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        take_bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                // Redirect only this call edge to the RetTake trampoline block.
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetTake expected a Call terminator"),
                    }
                }

                // Keep ref-ancestor in sync for return-tag recovery.
                // Without this, later PtrDerive on the returned pointer may pick an
                // uninitialized ref-ancestor local (0) and lose provenance, causing
                // OOB accesses to degrade into WILD_POINTER.
                if let Some(dst_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&dst_local).copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(dst_tag))),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&dst_local)
                    .copied()
                {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetAnchorTake { callee_id, local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorTake");

                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_tag,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorTake expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::RetLeafTake {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            if let InstrKind::RetAnchorRoot { local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorRoot");
                let local_ty = body.local_decls[local].ty;
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for RetAnchorRoot");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let ret_take_cont_bb = if slot_state.slot_family_valid_local.is_some() {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                } else {
                    orig_target
                };

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("RetAnchorRoot on unsupported local");
                let align_op = self.align_operand_for_ty(tcx, body, local_ty, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_raw,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u8(tcx, source_info.span, 1),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u8(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_usize(tcx, source_info.span, 0),
                                span: source_info.span,
                            },
                            Spanned {
                                node: arg_align,
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(slot_state.anchor_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(addr_stmt1);
                    take_bd.statements.push(addr_stmt2);
                    if !align_stmts.is_empty() {
                        take_bd.statements.append(&mut align_stmts);
                    }
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring RetAnchorRoot");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("RetAnchorRoot expected a Call terminator"),
                    }
                }
                if let Some(anchor_state_local) = slot_state.slot_family_valid_local {
                    body.basic_blocks_mut()[ret_take_cont_bb].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }

                continue;
            }

            if let InstrKind::MutArgRetLeafTake {
                callee_id,
                arg_index,
                local,
                leaf_key,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetLeafTake");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (base_addr_stmt1, base_addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        base_addr_local,
                        false,
                    )
                    .expect("MutArgRetLeafTake on unsupported local");
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_mut_arg_ret_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(base_addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(slot_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(orig_target),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };
                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    take_bd.statements.push(base_addr_stmt1);
                    take_bd.statements.push(base_addr_stmt2);
                    take_bd.statements.push(slot_addr_stmt1);
                    take_bd.statements.push(slot_addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };
                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetLeafTake");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetLeafTake expected a Call terminator"),
                    }
                }

                continue;
            }

            // Caller-side pointer-only writeback:
            // forwarders that receive `&mut T` directly do not have a separate carrier local in
            // this frame, but the live pointer local still needs the post-call family before any
            // later use or re-export.
            if let InstrKind::MutArgRetTakePtrOnly {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let (orig_target, call_source, fn_span) = {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator for MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call {
                            target,
                            call_source,
                            fn_span,
                            ..
                        } => {
                            let tgt = target.expect("call without target for MutArgRetTakePtrOnly");
                            (tgt, *call_source, *fn_span)
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                };
                let is_cleanup = body.basic_blocks[orig_target].is_cleanup;

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("MutArgRetTakePtrOnly on unsupported local");

                let dst_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_mut_arg_ret_tag_or_zero,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let is_zero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let nonzero_u64_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_keep_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let ptr_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let ret_take_cont_bb = {
                    let goto_term = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto {
                            target: orig_target,
                        },
                    });
                    body.basic_blocks_mut()
                        .push(BasicBlockData::new(goto_term, is_cleanup))
                };

                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(dst_tag_local),
                        target: Some(ret_take_cont_bb),
                        unwind: UnwindAction::Continue,
                        call_source,
                        fn_span,
                    },
                };

                let ret_take_bb = {
                    let mut take_bd = BasicBlockData::new(Some(take_term), is_cleanup);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        take_bd.statements.push(addr_stmt1);
                    }
                    take_bd.statements.push(addr_stmt2);
                    body.basic_blocks_mut().push(take_bd)
                };

                {
                    let term = body.basic_blocks_mut()[bb]
                        .terminator
                        .as_mut()
                        .expect("missing terminator while wiring MutArgRetTakePtrOnly");
                    match &mut term.kind {
                        TerminatorKind::Call { target, .. } => {
                            *target = Some(ret_take_bb);
                        }
                        _ => panic!("MutArgRetTakePtrOnly expected a Call terminator"),
                    }
                }

                if let Some(ptr_tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                    body.basic_blocks_mut()[ret_take_cont_bb]
                        .statements
                        .extend([
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Eq,
                                        Box::new((
                                            Operand::Copy(Place::from(dst_tag_local)),
                                            self.const_u64(tcx, source_info.span, 0),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(is_zero_u64_local),
                                    Rvalue::Cast(
                                        CastKind::IntToInt,
                                        Operand::Copy(Place::from(is_zero_local)),
                                        tcx.types.u64,
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(nonzero_u64_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Sub,
                                        Box::new((
                                            self.const_u64(tcx, source_info.span, 1),
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_keep_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(is_zero_u64_local)),
                                            Operand::Copy(Place::from(ptr_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_new_part_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Mul,
                                        Box::new((
                                            Operand::Copy(Place::from(nonzero_u64_local)),
                                            Operand::Copy(Place::from(dst_tag_local)),
                                        )),
                                    ),
                                ))),
                            ),
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(ptr_selected_local),
                                    Rvalue::BinaryOp(
                                        BinOp::Add,
                                        Box::new((
                                            Operand::Copy(Place::from(ptr_keep_part_local)),
                                            Operand::Copy(Place::from(ptr_new_part_local)),
                                        )),
                                    ),
                                ))),
                            ),
                        ]);

                    let ptr_ref_ancestor_local =
                        ref_ancestor_local_for_ptr_local.get(&ptr_local).copied();
                    let mut apply_bd = BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Goto {
                                target: orig_target,
                            },
                        }),
                        is_cleanup,
                    );
                    let ptr_tag_stmt_idx = apply_bd.statements.len();
                    apply_bd.statements.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ptr_tag_local),
                            Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                        ))),
                    ));
                    if let Some(export_parent_local) =
                        export_parent_local_for_ptr_local.get(&ptr_local).copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                    {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ));
                    }
                    if let Some(ptr_ref_ancestor_local) = ptr_ref_ancestor_local {
                        apply_bd.statements.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(ptr_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(ptr_selected_local))),
                            ))),
                        ));
                    }
                    let apply_bb = body.basic_blocks_mut().push(apply_bd);
                    manual_holder_managed_tag_assignments.insert((apply_bb, ptr_tag_stmt_idx));

                    let tmp_kill_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let kill_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_kill,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_tag_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_kill_unit),
                                target: Some(apply_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));

                    let tmp_retain_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let retain_bb = body.basic_blocks_mut().push(BasicBlockData::new(
                        Some(Terminator {
                            source_info,
                            kind: TerminatorKind::Call {
                                func: Operand::function_handle(
                                    tcx,
                                    hooks.def_id_tag_retain,
                                    std::iter::empty(),
                                    source_info.span,
                                ),
                                args: vec![Spanned {
                                    node: Operand::Copy(Place::from(ptr_selected_local)),
                                    span: source_info.span,
                                }]
                                .into_boxed_slice(),
                                destination: Place::from(tmp_retain_unit),
                                target: Some(kill_bb),
                                unwind: UnwindAction::Continue,
                                call_source: CallSource::Misc,
                                fn_span: source_info.span,
                            },
                        }),
                        is_cleanup,
                    ));
                    body.basic_blocks_mut()[ret_take_cont_bb].terminator = Some(Terminator {
                        source_info,
                        kind: TerminatorKind::Goto { target: retain_bb },
                    });
                }

                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::MutArgRetPush {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for MutArgRetPush");

                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );

                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let bd = &mut body.basic_blocks_mut()[bb];
                bd.statements.push(addr_stmt1);
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::MutArgRetLeafPush {
                callee_id,
                arg_index,
                ptr_local,
                leaf_key,
            } = creation_kind
            {
                let base_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let deref_place = Place::from(ptr_local).project_deeper(&[PlaceElem::Deref], tcx);
                let Some((base_addr_stmt1, base_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    deref_place,
                    base_addr_local,
                    false,
                ) else {
                    continue;
                };
                let slot_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((slot_addr_stmt1, slot_addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    slot_addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_mut_arg_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(base_addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(slot_addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let bd = &mut body.basic_blocks_mut()[bb];
                bd.statements.push(base_addr_stmt1);
                bd.statements.push(base_addr_stmt2);
                bd.statements.push(slot_addr_stmt1);
                bd.statements.push(slot_addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            // Callee-side: push the return anchor for a by-value direct-ref carrier immediately
            // before the `Return` terminator.
            if let InstrKind::RetAnchorPush { callee_id, local } = creation_kind {
                let slot_state = self
                    .carrier_slot_locals_for_local(
                        local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                    )
                    .expect("missing carrier-slot locals for RetAnchorPush");
                let borrow_kind = self
                    .first_direct_ref_field_place(tcx, body, local)
                    .map(|(_, _, is_mut)| {
                        if is_mut {
                            BorrowKind::Mut {
                                kind: MutBorrowKind::Default,
                            }
                        } else {
                            BorrowKind::Shared
                        }
                    })
                    .unwrap_or(BorrowKind::Shared);
                let mut anchor_select_stmts = Vec::new();
                let selected_anchor_local = self
                    .backtrack_unique_aggregate_anchor_source_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        local,
                        reborrow_anchor_local_for_stack_local,
                    )
                    .and_then(|src_local| {
                        let src_ty = body.local_decls[src_local].ty;
                        let src_tag_local = tag_local_for_ptr_local.get(&src_local).copied()?;
                        if !matches!(src_ty.kind(), TyKind::Ref(_, _, Mutability::Not)) {
                            return None;
                        }
                        Some(self.materialize_boundary_recovered_source_tag_local(
                            tcx,
                            body,
                            source_info,
                            src_local,
                            src_tag_local,
                            export_parent_local_for_ptr_local,
                            export_parent_is_recovered_local_for_ptr_local,
                            &mut anchor_select_stmts,
                        ))
                    })
                    .or_else(|| {
                        self.materialize_projectionless_slot_anchor_parent_local(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            Place::from(local),
                            borrow_kind,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            anchor_is_slot_family_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            &mut anchor_select_stmts,
                        )
                    })
                    .unwrap_or(slot_state.anchor_local);

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(selected_anchor_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    bd.statements.extend(anchor_select_stmts);
                    (bd.terminator.take(), bd.is_cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            // Callee-side: push the return tag immediately before the `Return` terminator.
            if let InstrKind::RetPush {
                callee_id,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for RetPush");

                // Use the data pointer for wide return values so tag passing stays consistent.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));

                // Use the same address extraction helper for wide pointers.
                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("RetPush on non-pointer local");

                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let selected_tag_local = if let (Some(export_parent_local), Some(recovered_local)) = (
                    export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&ptr_local)
                        .copied(),
                ) {
                    let recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let not_recovered_u64_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let fallback_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let export_part_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let selected_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let bd = &mut body.basic_blocks_mut()[bb];
                    bd.statements.extend([
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_u64_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(recovered_local)),
                                    tcx.types.u64,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(not_recovered_u64_local),
                                Rvalue::BinaryOp(
                                    BinOp::Sub,
                                    Box::new((
                                        self.const_u64(tcx, source_info.span, 1),
                                        Operand::Copy(Place::from(recovered_u64_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(fallback_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(not_recovered_u64_local)),
                                        Operand::Copy(Place::from(tag_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_part_local),
                                Rvalue::BinaryOp(
                                    BinOp::Mul,
                                    Box::new((
                                        Operand::Copy(Place::from(recovered_u64_local)),
                                        Operand::Copy(Place::from(export_parent_local)),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(selected_local),
                                Rvalue::BinaryOp(
                                    BinOp::Add,
                                    Box::new((
                                        Operand::Copy(Place::from(fallback_part_local)),
                                        Operand::Copy(Place::from(export_part_local)),
                                    )),
                                ),
                            ))),
                        ),
                    ]);
                    Some(selected_local)
                } else {
                    None
                };

                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(selected_tag_local.unwrap_or(tag_local))),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };

                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let bd = &mut body.basic_blocks_mut()[bb];
                // Put the address computation in the current block, then call push, then jump to old Return.
                if let Some(addr_stmt1) = addr_stmt1_opt {
                    bd.statements.push(addr_stmt1);
                }
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::RetValidate { callee_id, local } = creation_kind {
                let tag_op = self.parent_tag_operand_for_src_place(
                    tcx,
                    body,
                    bb,
                    stmt_idx,
                    source_info,
                    Place::from(local),
                    tag_local_for_ptr_local,
                    ref_ancestor_local_for_ptr_local,
                    reborrow_anchor_local_for_stack_local,
                    projectionless_anchor_suppressed_locals,
                    false,
                    true,
                    ParentSelectionMode::PointeeFamily,
                );
                let validate_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_validate_ret_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_validate: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: tag_op,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: validate_func,
                        args: args_validate,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
                continue;
            }

            if let InstrKind::RetLeafPush {
                callee_id,
                leaf_key,
            } = creation_kind
            {
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    addr_local,
                    false,
                ) else {
                    continue;
                };
                let push_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_push_ret_leaf_shadow,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_push: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, leaf_key),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: push_func,
                        args: args_push,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let bd = &mut body.basic_blocks_mut()[bb];
                bd.statements.push(addr_stmt1);
                bd.statements.push(addr_stmt2);
                bd.terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagProp {
                dst,
                src,
                copy_tag,
                copy_ref_ancestor,
            } = creation_kind
            {
                let prop_stmt = if copy_tag {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for TagProp dst");

                    let src_op: Operand<'tcx> =
                        if let Some(src_tag) = tag_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*src_tag))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };

                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_tag),
                            Rvalue::Use(src_op),
                        ))),
                    ))
                } else {
                    None
                };

                let prop_ref_ancestor_stmt = if copy_ref_ancestor {
                    let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for TagProp dst");
                    let src_ref_ancestor_op: Operand<'tcx> = if let Some(src_ref_ancestor) =
                        ref_ancestor_local_for_ptr_local.get(&src)
                    {
                        Operand::Copy(Place::from(*src_ref_ancestor))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                    Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_ref_ancestor),
                            Rvalue::Use(src_ref_ancestor_op),
                        ))),
                    ))
                } else {
                    None
                };

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut next_insert = insert_at;
                if let Some(prop_stmt) = prop_stmt {
                    bd.statements.insert(next_insert, prop_stmt);
                    next_insert += 1;
                }
                if let Some(prop_ref_ancestor_stmt) = prop_ref_ancestor_stmt {
                    bd.statements.insert(next_insert, prop_ref_ancestor_stmt);
                    next_insert += 1;
                }
                if copy_tag {
                    if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                        export_parent_local_for_ptr_local.get(&dst).copied(),
                        export_parent_local_for_ptr_local.get(&src).copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_export_parent_local),
                                    Rvalue::Use(Operand::Copy(Place::from(
                                        src_export_parent_local,
                                    ))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                    if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&dst)
                            .copied(),
                        export_parent_is_recovered_local_for_ptr_local
                            .get(&src)
                            .copied(),
                    ) {
                        bd.statements.insert(
                            next_insert,
                            Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_recovered_local),
                                    Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                                ))),
                            ),
                        );
                        next_insert += 1;
                    }
                }
                continue;
            }

            if let InstrKind::TagPropFromRefAncestor { dst, src } = creation_kind {
                let dst_tag = *tag_local_for_ptr_local
                    .get(&dst)
                    .expect("missing tag local for TagPropFromRefAncestor dst");
                let dst_ref_ancestor = *ref_ancestor_local_for_ptr_local
                    .get(&dst)
                    .expect("missing ref-ancestor local for TagPropFromRefAncestor dst");
                let src_ref_ancestor_op: Operand<'tcx> =
                    if let Some(src_ref_ancestor) = ref_ancestor_local_for_ptr_local.get(&src) {
                        Operand::Copy(Place::from(*src_ref_ancestor))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };

                let tag_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_tag),
                        Rvalue::Use(src_ref_ancestor_op.clone()),
                    ))),
                );
                let ref_ancestor_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(dst_ref_ancestor),
                        Rvalue::Use(src_ref_ancestor_op),
                    ))),
                );

                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut prop_stmts = vec![tag_stmt, ref_ancestor_stmt];
                if let (Some(dst_export_parent_local), Some(src_export_parent_local)) = (
                    export_parent_local_for_ptr_local.get(&dst).copied(),
                    export_parent_local_for_ptr_local.get(&src).copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_export_parent_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_export_parent_local))),
                        ))),
                    ));
                }
                if let (Some(dst_recovered_local), Some(src_recovered_local)) = (
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&dst)
                        .copied(),
                    export_parent_is_recovered_local_for_ptr_local
                        .get(&src)
                        .copied(),
                ) {
                    prop_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(dst_recovered_local),
                            Rvalue::Use(Operand::Copy(Place::from(src_recovered_local))),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, prop_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorZero {
                anchor_local,
                anchor_state_local,
            } = creation_kind
            {
                let mut zero_stmts = vec![Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                    ))),
                )];
                if let Some(anchor_state_local) = anchor_state_local {
                    zero_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.splice(insert_at..insert_at, zero_stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSet {
                dst_local: _dst_local,
                anchor_local,
                anchor_state_local,
                src_ptr_local,
            } = creation_kind
            {
                let src_tag_operand = if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_ptr_local).copied()
                {
                    Operand::Copy(Place::from(src_tag_local))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_ptr_local)
                    .copied()
                {
                    Operand::Copy(Place::from(src_anchor_local))
                } else {
                    panic!("missing lineage source for ReborrowAnchorSet src");
                };
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let anchor_is_zero_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_is_zero_local),
                        Rvalue::BinaryOp(
                            BinOp::Eq,
                            Box::new((
                                Operand::Copy(Place::from(anchor_local)),
                                self.const_u64(tcx, source_info.span, 0),
                            )),
                        ),
                    ))),
                );
                let anchor_should_init_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_should_init_local),
                        Rvalue::Cast(
                            CastKind::IntToInt,
                            Operand::Copy(Place::from(anchor_is_zero_local)),
                            tcx.types.u64,
                        ),
                    ))),
                );
                let anchor_new_part_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_new_part_local),
                        Rvalue::BinaryOp(
                            BinOp::Mul,
                            Box::new((
                                Operand::Copy(Place::from(anchor_should_init_local)),
                                src_tag_operand,
                            )),
                        ),
                    ))),
                );
                let anchor_selected_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_selected_local),
                        Rvalue::BinaryOp(
                            BinOp::Add,
                            Box::new((
                                Operand::Copy(Place::from(anchor_local)),
                                Operand::Copy(Place::from(anchor_new_part_local)),
                            )),
                        ),
                    ))),
                );
                let set_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(anchor_local),
                        Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let mut stmts = vec![
                    anchor_is_zero_stmt,
                    anchor_should_init_stmt,
                    anchor_new_part_stmt,
                    anchor_selected_stmt,
                    set_stmt,
                ];
                if let Some(anchor_state_local) = anchor_state_local {
                    stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_state_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                        ))),
                    ));
                }
                bd.statements.splice(insert_at..insert_at, stmts);
                continue;
            }

            if let InstrKind::ReborrowAnchorSeed {
                dst_local,
                src_local,
                mark_slot_family,
            } = creation_kind
            {
                let mut src_tag_select_stmts = Vec::new();
                let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&dst_local)
                    .copied()
                else {
                    continue;
                };
                let Some(src_tag_operand) = (if let Some(src_tag_local) =
                    tag_local_for_ptr_local.get(&src_local).copied()
                {
                    let selected_src_tag_local = self
                        .materialize_boundary_recovered_source_tag_local(
                            tcx,
                            body,
                            source_info,
                            src_local,
                            src_tag_local,
                            export_parent_local_for_ptr_local,
                            export_parent_is_recovered_local_for_ptr_local,
                            &mut src_tag_select_stmts,
                        );
                    Some(Operand::Copy(Place::from(selected_src_tag_local)))
                } else if let Some(src_anchor_local) = reborrow_anchor_local_for_stack_local
                    .get(&src_local)
                    .copied()
                {
                    Some(Operand::Copy(Place::from(src_anchor_local)))
                } else {
                    None
                }) else {
                    continue;
                };
                let dst_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&dst_local)
                    .copied();
                let src_anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&src_local)
                    .copied();
                let anchor_is_zero_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                let anchor_should_init_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_new_part_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let anchor_selected_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                let inserted_stmt_count = src_tag_select_stmts.len() + 5;
                let seed_stmts = [
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_is_zero_local),
                            Rvalue::BinaryOp(
                                BinOp::Eq,
                                Box::new((
                                    Operand::Copy(Place::from(anchor_local)),
                                    self.const_u64(tcx, source_info.span, 0),
                                )),
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_should_init_local),
                            Rvalue::Cast(
                                CastKind::IntToInt,
                                Operand::Copy(Place::from(anchor_is_zero_local)),
                                tcx.types.u64,
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_new_part_local),
                            Rvalue::BinaryOp(
                                BinOp::Mul,
                                Box::new((
                                    Operand::Copy(Place::from(anchor_should_init_local)),
                                    src_tag_operand,
                                )),
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_selected_local),
                            Rvalue::BinaryOp(
                                BinOp::Add,
                                Box::new((
                                    Operand::Copy(Place::from(anchor_local)),
                                    Operand::Copy(Place::from(anchor_new_part_local)),
                                )),
                            ),
                        ))),
                    ),
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(anchor_local),
                            Rvalue::Use(Operand::Copy(Place::from(anchor_selected_local))),
                        ))),
                    ),
                ];
                bd.statements.splice(
                    insert_at..insert_at,
                    src_tag_select_stmts.into_iter().chain(seed_stmts),
                );
                if let Some(dst_anchor_state_local) = dst_anchor_state_local {
                    let state_stmt = if mark_slot_family {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        )
                    } else if let Some(src_anchor_state_local) = src_anchor_state_local {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(Operand::Copy(Place::from(src_anchor_state_local))),
                            ))),
                        )
                    } else {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                            ))),
                        )
                    };
                    bd.statements
                        .insert(insert_at + inserted_stmt_count, state_stmt);
                }
                continue;
            }

            if let InstrKind::ParentTagSnapshot {
                dst_local,
                src,
                is_raw_creation,
            } = creation_kind
            {
                let parent_local = *ref_ancestor_local_for_ptr_local
                    .get(&dst_local)
                    .expect("missing ref-ancestor local for ParentTagSnapshot dst");
                let parent_stmt = Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(parent_local),
                        Rvalue::Use(self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            src,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            is_raw_creation,
                            true,
                            self.creation_parent_selection_mode_for_src_place(
                                tcx,
                                body,
                                src,
                                is_raw_creation,
                                true,
                            ),
                        )),
                    ))),
                );
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let insert_at = if stmt_idx >= bd.statements.len() {
                    bd.statements.len()
                } else {
                    stmt_idx + 1
                };
                bd.statements.insert(insert_at, parent_stmt);
                continue;
            }

            if let InstrKind::ArgRetag {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for ArgRetag");

                // Take the caller-pushed tag first, then create a fresh tag for this argument.
                let (record_def_id, is_mut_u8) = match body.local_decls[ptr_local].ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_ref, if is_mut { 1 } else { 0 })
                    }
                    TyKind::RawPtr(_ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_raw, if is_mut { 1 } else { 0 })
                    }
                    _ => panic!("ArgRetag on non-pointer local"),
                };
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        // Argument retagging often introduces short-lived receiver borrows at
                        // call boundaries. If source-tag plumbing drops the parent, allow runtime
                        // same-address repair instead of creating a sibling root.
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let bounds_len_op =
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };

                let record_func = Operand::function_handle(
                    tcx,
                    record_def_id,
                    std::iter::empty(),
                    source_info.span,
                );
                let record_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: record_func,
                        args: args_record,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let retag_block = {
                    let retag_data = BasicBlockData::new(Some(record_term), is_cleanup);
                    body.basic_blocks_mut().push(retag_data)
                };

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_call_arg_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(parent_tag_local),
                        target: Some(retag_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        // Shared refs at function entry are boundary-recovered values. Preserve
                        // the incoming boundary parent for later call/return transport.
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, recovered_value)),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&local)
                    .copied();
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(local),
                        addr_local,
                        false,
                    )
                    .expect("ArgAnchorTake on unsupported local");
                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, callee_id),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, arg_index),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_call_arg_tag_anchor,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(anchor_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    bd.terminator = Some(take_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                if let Some(anchor_state_local) = anchor_state_local {
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, 1)),
                            ))),
                        ),
                    );
                }
                continue;
            }

            if let InstrKind::FnExit { callee_id } = creation_kind {
                let exit_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_exit_fn,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_exit: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: self.const_u64(tcx, source_info.span, callee_id),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: exit_func,
                        args: args_exit,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagKill { ptr_local } = creation_kind {
                let Some(tag_local) = tag_local_for_ptr_local.get(&ptr_local).copied() else {
                    continue;
                };
                let mut tag_locals: Vec<Local> = vec![tag_local];
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    if export_parent_local != tag_local {
                        tag_locals.push(export_parent_local);
                    }
                }

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let mut next_target = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let cont_block = next_target;
                for kill_local in tag_locals.into_iter().rev() {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let call_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_tag_kill,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![Spanned {
                                node: Operand::Copy(Place::from(kill_local)),
                                span: source_info.span,
                            }]
                            .into_boxed_slice(),
                            destination: Place::from(tmp_unit),
                            target: Some(next_target),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    next_target = body
                        .basic_blocks_mut()
                        .push(BasicBlockData::new(Some(call_term), is_cleanup));
                }
                let mut reset_stmts = Vec::new();
                reset_stmts.push(Statement::new(
                    source_info,
                    StatementKind::Assign(Box::new((
                        Place::from(tag_local),
                        Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                    ))),
                ));
                if let Some(ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(ref_ancestor_local),
                            Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(export_parent_local),
                            Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    reset_stmts.push(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(recovered_local),
                            Rvalue::Use(self.const_u8(tcx, source_info.span, 0)),
                        ))),
                    ));
                }
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, reset_stmts);
                body.basic_blocks_mut()[bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Goto {
                        target: next_target,
                    },
                });
                continue;
            }

            if let InstrKind::TagLocalKill { tag_local } = creation_kind {
                let kill_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_kill,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_kill: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: kill_func,
                        args: args_kill,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            if let InstrKind::TagRetain { tag_local } = creation_kind {
                let retain_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_tag_retain,
                    std::iter::empty(),
                    source_info.span,
                );
                let args_retain: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                    node: Operand::Copy(Place::from(tag_local)),
                    span: source_info.span,
                }]
                .into_boxed_slice();

                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    (bd.terminator.take(), bd.is_cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));

                let call_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: retain_func,
                        args: args_retain,
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                body.basic_blocks_mut()[bb].terminator = Some(call_term);
                continue;
            }

            let mut func_operand =
                self.func_operand_for(tcx, hooks, &creation_kind, source_info.span);
            if let InstrKind::ShadowStore { src_local } = creation_kind {
                if self.shadow_store_uses_local_slot_store(
                    local_slot_shadow_store_locals,
                    place,
                    src_local,
                ) {
                    func_operand = Operand::function_handle(
                        tcx,
                        hooks.def_id_shadow_store_ptr_local,
                        std::iter::empty(),
                        source_info.span,
                    );
                }
            }

            let insert_before: bool = ip.insert_before;

            let tag_local: Option<Local> = match creation_kind {
                InstrKind::Ref { .. } | InstrKind::Raw { .. } => {
                    if let Some(lhs_local) = place.as_local() {
                        Some(
                            *tag_local_for_ptr_local
                                .get(&lhs_local)
                                .expect("missing preallocated tag local for pointer destination"),
                        )
                    } else {
                        None
                    }
                }
                InstrKind::RetRoot { dst_local, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst_local)
                        .expect("missing preallocated tag local for RetRoot destination"),
                ),
                InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => Some(
                    *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing preallocated tag local for PtrDerive destination"),
                ),
                _ => None,
            };

            let addr_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));

            // Compute the address for runtime hooks. We route all pointer cases through
            // addr_stmts_for_place so wide pointers are handled via their data pointer.
            let mut addr_extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            let (addr_stmt1_opt, addr_stmt2) = match creation_kind {
                InstrKind::StackAlloc { local, .. } => {
                    let local_ty = body.local_decls[local].ty;
                    // Stack-slot tracking only needs the numeric address of a concrete local in
                    // this MIR body. Opaque/generic-but-sized locals (for example
                    // `impl RangeBounds<usize>` in optimized `bytes::Bytes::slice`) are still
                    // valid `&raw const local` sources even though `is_thin_ptr_ty` rejects them
                    // for generic pointer-value exposure.
                    if !local_ty.is_sized(tcx, body.typing_env(tcx)) {
                        continue;
                    }
                    let ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(ptr_ty, source_info.span));

                    let s1 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::RawPtr(RawPtrKind::Const, Place::from(local)),
                        ))),
                    );

                    let s2 = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Cast(
                                CastKind::PointerExposeProvenance,
                                Operand::Copy(Place::from(tmp_ptr)),
                                tcx.types.usize,
                            ),
                        ))),
                    );

                    (Some(s1), s2)
                }
                // Heap/const allocations already have a pointer local; just expose its address.
                InstrKind::HeapAlloc { ptr_local, .. }
                | InstrKind::ConstAlloc { ptr_local, .. } => {
                    match self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    ) {
                        Some(stmts) => stmts,
                        None => continue,
                    }
                }
                InstrKind::ConstAllocConst { const_op, .. } => {
                    let const_ty = const_op.const_.ty();
                    let tmp_ptr = body
                        .local_decls
                        .push(LocalDecl::new(const_ty, source_info.span));
                    let assign_const = Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(tmp_ptr),
                            Rvalue::Use(Operand::Constant(Box::new(const_op.clone()))),
                        ))),
                    );
                    let Some((opt_stmt, addr_stmt)) = self.addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(tmp_ptr),
                        addr_local,
                    ) else {
                        continue;
                    };
                    // If we need multiple address statements (wide pointer), skip.
                    if opt_stmt.is_some() {
                        continue;
                    }
                    (Some(assign_const), addr_stmt)
                }
                InstrKind::PtrRead { .. }
                | InstrKind::PtrWrite { .. }
                | InstrKind::PtrReadAllowUntagged { .. }
                | InstrKind::PtrWriteAllowUntagged { .. } => {
                    if let Some((opt, stmt, offset_stmts)) =
                        self.addr_stmts_for_access_place(tcx, body, source_info, place, addr_local)
                    {
                        addr_extra_stmts = offset_stmts;
                        (opt, stmt)
                    } else {
                        // Fallback for projection-heavy deref accesses where static offset
                        // recovery failed (e.g., generic field layout):
                        // materialize `&raw {const,mut} <full place>` and expose that pointer.
                        // Using only `place.local` here points at the base carrier and can
                        // turn valid projected accesses into false OOB/WILD reports.
                        let place_ty = place.ty(&body.local_decls, tcx).ty;
                        let is_write = matches!(
                            creation_kind,
                            InstrKind::PtrWrite { .. } | InstrKind::PtrWriteAllowUntagged { .. }
                        );
                        let raw_ptr_ty = if is_write {
                            Ty::new_mut_ptr(tcx, place_ty)
                        } else {
                            Ty::new_imm_ptr(tcx, place_ty)
                        };
                        if !self.is_addr_exposable_ptr_ty(tcx, body, raw_ptr_ty) {
                            continue;
                        }
                        let tmp_ptr = body
                            .local_decls
                            .push(LocalDecl::new(raw_ptr_ty, source_info.span));
                        let raw_ptr_stmt = Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(tmp_ptr),
                                Rvalue::RawPtr(
                                    if is_write {
                                        RawPtrKind::Mut
                                    } else {
                                        RawPtrKind::Const
                                    },
                                    place,
                                ),
                            ))),
                        );
                        match self.addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(tmp_ptr),
                            addr_local,
                        ) {
                            Some((opt_stmt, addr_stmt)) => {
                                if let Some(s) = opt_stmt {
                                    addr_extra_stmts.push(s);
                                }
                                (Some(raw_ptr_stmt), addr_stmt)
                            }
                            None => continue,
                        }
                    }
                }
                InstrKind::StackSlotWriteAllowUntagged { .. } => {
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        true,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::CallArgPush { ptr_local, .. } => {
                    let place_ty = place.ty(&body.local_decls, tcx).ty;
                    if self.is_pointer_ty(place_ty) {
                        match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                            Some(stmts) => stmts,
                            None => continue,
                        }
                    } else {
                        let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            place,
                            addr_local,
                            false,
                        ) else {
                            continue;
                        };
                        (Some(slot_stmt1), slot_stmt2)
                    }
                }
                InstrKind::CallArgLeafPush { .. } | InstrKind::ArgLeafTake { .. } => {
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        false,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::CallArgValidate { .. } | InstrKind::RetValidate { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. } => {
                    let is_mut = matches!(
                        creation_kind,
                        InstrKind::ShadowStore { .. } | InstrKind::ShadowKill { .. }
                    );
                    let Some((slot_stmt1, slot_stmt2)) = self.slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        addr_local,
                        is_mut,
                    ) else {
                        continue;
                    };
                    (Some(slot_stmt1), slot_stmt2)
                }
                InstrKind::TagKill { .. }
                | InstrKind::TagLocalKill { .. }
                | InstrKind::TagRetain { .. } => (
                    None,
                    Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(addr_local),
                            Rvalue::Use(self.const_usize(tcx, source_info.span, 0)),
                        ))),
                    ),
                ),
                _ => match self.addr_stmts_for_place(tcx, body, source_info, place, addr_local) {
                    Some(stmts) => stmts,
                    None => continue,
                },
            };

            let (orig_term, is_cleanup) = {
                let bd = &mut body.basic_blocks_mut()[bb];
                let term = bd.terminator.take();
                let cleanup = bd.is_cleanup;
                (term, cleanup)
            };

            let cont_block = {
                let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                body.basic_blocks_mut().push(cont_data)
            };

            let orig_stmt_len = orig_stmt_prefix_len
                .get(&bb)
                .copied()
                .unwrap_or_else(|| body.basic_blocks[bb].statements.len());

            let heap_alloc_info = match &creation_kind {
                InstrKind::HeapAlloc {
                    ptr_local, live, ..
                } => Some((*ptr_local, *live)),
                _ => None,
            };

            // For const/global allocations, rewrite the exposed pointer address to the base.
            let mut arg_addr_local = addr_local;
            let mut addr_adjust_stmt_opt: Option<Statement<'tcx>> = None;

            if let InstrKind::ConstAlloc { base_offset, .. }
            | InstrKind::ConstAllocConst { base_offset, .. } = &creation_kind
            {
                if *base_offset != 0 {
                    let base_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let offset_op = self.const_usize(tcx, source_info.span, *base_offset);
                    addr_adjust_stmt_opt = Some(Statement::new(
                        source_info,
                        StatementKind::Assign(Box::new((
                            Place::from(base_local),
                            Rvalue::BinaryOp(
                                BinOp::Sub,
                                Box::new((Operand::Copy(Place::from(addr_local)), offset_op)),
                            ),
                        ))),
                    ));
                    arg_addr_local = base_local;
                }
            }

            let arg_addr = Operand::Copy(Place::from(arg_addr_local));

            let mut extra_stmts: Vec<Statement<'tcx>> = Vec::new();
            if !addr_extra_stmts.is_empty() {
                extra_stmts.extend(addr_extra_stmts);
            }
            let projected_ref_parent_local = match &creation_kind {
                InstrKind::Ref {
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } if !src.projection.is_empty() => self
                    .materialize_projected_reborrow_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        projected_reborrow_anchor_key.as_ref(),
                        projected_reborrow_anchor_local_for_key,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    ),
                _ => None,
            };
            let projectionless_ref_parent_local = match &creation_kind {
                InstrKind::Ref { bk, src, .. }
                    if src.projection.is_empty()
                        && projectionless_anchor_suppressed_locals.contains(&src.local) =>
                {
                    self.materialize_projectionless_slot_anchor_parent_local(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        *src,
                        *bk,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        anchor_is_slot_family_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        &mut extra_stmts,
                    )
                }
                _ => None,
            };

            let (args, dest_place) = match creation_kind {
                InstrKind::PtrRead {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrReadAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // Projected accesses like `(*self_ref).field`, `slice[i]`, or
                        // `(*wide_ref).0` must validate in the family of the accessed
                        // projection source, not blindly in the family of the coarse
                        // `ptr_local` chosen during scan. `PtrUse` already does this;
                        // keep read validation consistent to avoid reusing an outer
                        // `&Bytes` tag on the empty-slice sentinel pointer `0x1`.
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackAlloc {
                    ref size_op, live, ..
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    // Encode stack-alloc flag in bit1; bit0 is the live flag.
                    let live_bits: u8 = if live { 1 } else { 0 };
                    let arg_live = self.const_u8(tcx, source_info.span, live_bits | 0x2);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::HeapAlloc {
                    ptr_local: _,
                    live,
                    ref size_op,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let arg_live = self.const_u8(tcx, source_info.span, if live { 1 } else { 0 });

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                // Record a live global/promoted allocation at the computed base address.
                InstrKind::ConstAlloc { size, .. } | InstrKind::ConstAllocConst { size, .. } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let arg_size = self.const_usize(tcx, source_info.span, size);
                    // __rz_record_alloc live bitfield:
                    // bit0 = live, bit1 = stack, bit2 = global/promoted const
                    let arg_live = self.const_u8(tcx, source_info.span, 0b101);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_live,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrWrite {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                }
                | InstrKind::PtrWriteAllowUntagged {
                    ptr_local,
                    ref size_op,
                    ref align_op,
                } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            self.parent_selection_mode_for_src_place(body, place),
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, align_op);
                    extra_stmts.append(&mut align_stmts);
                    let access_alias = self.const_u8(
                        tcx,
                        source_info.span,
                        self.alias_exempt_for_access_ty(
                            tcx,
                            body,
                            place.ty(&body.local_decls, tcx).ty,
                        ) as u8,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                        Spanned {
                            node: access_alias,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::StackSlotWriteAllowUntagged {
                    local, ref size_op, ..
                } => {
                    let Some(tag_local) =
                        reborrow_anchor_local_for_stack_local.get(&local).copied()
                    else {
                        continue;
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: Operand::Copy(Place::from(tag_local)),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgPush {
                    callee_id,
                    arg_index,
                    ptr_local,
                    parent_mode,
                    flags,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let fallback_tag_op: Operand<'tcx> = if place.projection.is_empty()
                        && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                    {
                        let pointee_anchor_local = self
                            .backtrack_pointer_pointee_local(
                                body,
                                ptr_local,
                                &body.basic_blocks[bb].statements,
                            )
                            .and_then(|pointee_local| {
                                if self.is_pointer_ty(body.local_decls[pointee_local].ty) {
                                    None
                                } else {
                                    reborrow_anchor_local_for_stack_local
                                        .get(&pointee_local)
                                        .copied()
                                }
                            });
                        if let Some(anchor_local) = pointee_anchor_local {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                let tag_is_zero_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                let tag_is_zero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let tag_is_nonzero_u64_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let keep_tag_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let anchor_part_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                let selected_tag_local = body
                                    .local_decls
                                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                extra_stmts.extend([
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Eq,
                                                Box::new((
                                                    Operand::Copy(Place::from(tl)),
                                                    self.const_u64(tcx, source_info.span, 0),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_zero_u64_local),
                                            Rvalue::Cast(
                                                CastKind::IntToInt,
                                                Operand::Copy(Place::from(tag_is_zero_local)),
                                                tcx.types.u64,
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(tag_is_nonzero_u64_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Sub,
                                                Box::new((
                                                    self.const_u64(tcx, source_info.span, 1),
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(keep_tag_part_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Mul,
                                                Box::new((
                                                    Operand::Copy(Place::from(
                                                        tag_is_nonzero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(tl)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(anchor_part_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Mul,
                                                Box::new((
                                                    Operand::Copy(Place::from(
                                                        tag_is_zero_u64_local,
                                                    )),
                                                    Operand::Copy(Place::from(anchor_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(selected_tag_local),
                                            Rvalue::BinaryOp(
                                                BinOp::Add,
                                                Box::new((
                                                    Operand::Copy(Place::from(keep_tag_part_local)),
                                                    Operand::Copy(Place::from(anchor_part_local)),
                                                )),
                                            ),
                                        ))),
                                    ),
                                ]);
                                Operand::Copy(Place::from(selected_tag_local))
                            } else {
                                Operand::Copy(Place::from(anchor_local))
                            }
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                            Operand::Copy(Place::from(tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        // For projected call arguments such as subslices (`output[a..b]`) or
                        // field projections, pushing the carrier local's current tag is often too
                        // weak: optimized MIR can keep only a wrapper temp tagged while the actual
                        // projected argument never materializes its own stable tag before the call.
                        //
                        // The callee only needs a parent lineage to retag its local argument at
                        // the callee address. Reuse the same parent-selection logic we use for
                        // ref/raw creation so interprocedural retagging stays attached to the
                        // source borrow family instead of falling back to a root inside the callee.
                        if matches!(place.projection.first(), Some(ProjectionElem::Deref))
                            && self.is_pointer_ty(body.local_decls[ptr_local].ty)
                        {
                            if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local).copied() {
                                Operand::Copy(Place::from(tl))
                            } else if let Some(tl) =
                                ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                            {
                                Operand::Copy(Place::from(tl))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    place,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    true,
                                    parent_mode,
                                )
                            }
                        } else {
                            self.parent_tag_operand_for_src_place(
                                tcx,
                                body,
                                bb,
                                stmt_idx,
                                source_info,
                                place,
                                tag_local_for_ptr_local,
                                ref_ancestor_local_for_ptr_local,
                                reborrow_anchor_local_for_stack_local,
                                projectionless_anchor_suppressed_locals,
                                false,
                                true,
                                parent_mode,
                            )
                        }
                    };
                    let exact_tag_op = fallback_tag_op;
                    let (boundary_parent_op, boundary_origin_op): (Operand<'tcx>, Operand<'tcx>) =
                        if let (Some(export_parent_local), Some(recovered_local)) = (
                            export_parent_local_for_ptr_local.get(&ptr_local).copied(),
                            export_parent_is_recovered_local_for_ptr_local
                                .get(&ptr_local)
                                .copied(),
                        ) {
                            (
                                Operand::Copy(Place::from(export_parent_local)),
                                Operand::Copy(Place::from(recovered_local)),
                            )
                        } else {
                            (
                                exact_tag_op.clone(),
                                self.const_u8(
                                    tcx,
                                    source_info.span,
                                    CALL_ARG_BOUNDARY_ORIGIN_EXACT,
                                ),
                            )
                        };

                    let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                    let arg_index = self.const_u64(tcx, source_info.span, arg_index);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_callee,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_index,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: exact_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: boundary_parent_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: boundary_origin_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u8(tcx, source_info.span, flags),
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgValidate { local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let tag_op = self.parent_tag_operand_for_src_place(
                        tcx,
                        body,
                        bb,
                        stmt_idx,
                        source_info,
                        place,
                        tag_local_for_ptr_local,
                        ref_ancestor_local_for_ptr_local,
                        reborrow_anchor_local_for_stack_local,
                        projectionless_anchor_suppressed_locals,
                        false,
                        true,
                        ParentSelectionMode::PointeeFamily,
                    );

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();

                    let _ = local;
                    (args, Place::from(tmp_unit))
                }

                InstrKind::CallArgLeafPush {
                    callee_id,
                    arg_index,
                    leaf_key,
                }
                | InstrKind::ArgLeafTake {
                    callee_id,
                    arg_index,
                    leaf_key,
                } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, callee_id),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, arg_index),
                            span: source_info.span,
                        },
                        Spanned {
                            node: self.const_u64(tcx, source_info.span, leaf_key),
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrUse { ptr_local } => {
                    let tag_op: Operand<'tcx> = if place.projection.is_empty() {
                        if let Some(debug_tag_local) = self.active_debug_ref_binding_tag_local(
                            body,
                            source_info.scope,
                            source_info.span,
                            ptr_local,
                            debug_ref_bindings,
                        ) {
                            Operand::Copy(Place::from(debug_tag_local))
                        } else if let Some(tl) = tag_local_for_ptr_local.get(&ptr_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        }
                    } else {
                        self.parent_tag_operand_for_src_place(
                            tcx,
                            body,
                            bb,
                            stmt_idx,
                            source_info,
                            place,
                            tag_local_for_ptr_local,
                            ref_ancestor_local_for_ptr_local,
                            reborrow_anchor_local_for_stack_local,
                            projectionless_anchor_suppressed_locals,
                            false,
                            true,
                            ParentSelectionMode::PointeeFamily,
                        )
                    };
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tmp_unit))
                }

                InstrKind::DebugRefActivate {
                    raw_local,
                    tag_local,
                    is_mut,
                } => {
                    let raw_tag_op = if let Some(tl) = tag_local_for_ptr_local.get(&raw_local) {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u64(tcx, source_info.span, 0)
                    };
                    let ref_ancestor_op =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&raw_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let alias_exempt =
                        self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[raw_local].ty);
                    let arg_alias =
                        self.const_u8(tcx, source_info.span, if alias_exempt { 1 } else { 0 });
                    let bounds_len_op = self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        raw_local,
                        source_info.span,
                    );
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, raw_local, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: raw_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(tag_local))
                }

                InstrKind::ShadowStore { src_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let ref_ancestor_op: Operand<'tcx> =
                        if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };
                    let export_parent_op: Operand<'tcx> =
                        if let Some(tl) = export_parent_local_for_ptr_local.get(&src_local) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            tag_op.clone()
                        };
                    let recovered_op: Operand<'tcx> = if let Some(tl) =
                        export_parent_is_recovered_local_for_ptr_local.get(&src_local)
                    {
                        Operand::Copy(Place::from(*tl))
                    } else {
                        self.const_u8(tcx, source_info.span, 0)
                    };
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: ref_ancestor_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: export_parent_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: recovered_op,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::ShadowKill { ref size_op } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let (arg_size, mut size_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, size_op);
                    extra_stmts.append(&mut size_stmts);
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_size,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagKill { ptr_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let tag_op = tag_local_for_ptr_local
                        .get(&ptr_local)
                        .copied()
                        .map(|tag_local| Operand::Copy(Place::from(tag_local)))
                        .unwrap_or_else(|| self.const_u64(tcx, source_info.span, 0));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: tag_op,
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagLocalKill { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::TagRetain { tag_local } => {
                    let tmp_unit = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, source_info.span));
                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                        node: Operand::Copy(Place::from(tag_local)),
                        span: source_info.span,
                    }]
                    .into_boxed_slice();
                    (args, Place::from(tmp_unit))
                }

                InstrKind::PtrDerive {
                    dst,
                    src,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDerive dst");

                    let parent_from_src: Operand<'tcx> =
                        if let Some(tl) = tag_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*tl))
                        } else if let Some(tl) = ref_ancestor_local_for_ptr_local.get(&src) {
                            Operand::Copy(Place::from(*tl))
                        } else {
                            self.const_u64(tcx, source_info.span, 0)
                        };

                    let parent_tag_op: Operand<'tcx> = match &creation_kind {
                        // For derivations, use the source local's current tag when available.
                        // Falling back to ref-ancestor is only a backup path.
                        InstrKind::PtrDerive { is_ref: true, .. } => parent_from_src,
                        _ => {
                            // Keep raw derivations connected to source provenance.
                            parent_from_src
                        }
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    // Bitfield semantics match __record_* hooks:
                    // bit0=alias_exempt, bit1=basic lineage-repair hint, bit2=strong hint,
                    // bit3=carry wide bounds from src when derivation drops metadata.
                    // Raw PtrDerive in optimized MIR often comes from projection-heavy lowering
                    // and benefits from runtime parent repair when stack metadata is coarse.
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if self.should_forward_bounds_from_src_ptr_derive(tcx, body, src, dst) {
                            alias_flags |= 0b1000;
                        }
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = self.align_operand_for_ptr_derive(
                        tcx,
                        body,
                        src,
                        dst,
                        is_ref,
                        source_info.span,
                    );
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                InstrKind::PtrDeriveParent {
                    dst,
                    is_mut,
                    is_ref,
                    strict_validity,
                } => {
                    let dst_tag = *tag_local_for_ptr_local
                        .get(&dst)
                        .expect("missing tag local for PtrDeriveParent dst");
                    let parent_local = *ref_ancestor_local_for_ptr_local
                        .get(&dst)
                        .expect("missing ref-ancestor local for PtrDeriveParent dst");

                    let parent_tag_op: Operand<'tcx> = Operand::Copy(Place::from(parent_local));

                    let arg_mut = self.const_u8(tcx, source_info.span, if is_mut { 1 } else { 0 });
                    let dst_ty = body.local_decls[dst].ty;
                    let alias_exempt = self.alias_exempt_for_ptr_ty(tcx, body, dst_ty);
                    let mut alias_flags: u8 = if alias_exempt { 1 } else { 0 };
                    if !is_ref {
                        alias_flags |= 0b10 | 0b100;
                        if strict_validity {
                            alias_flags |= 0b0100_0000;
                        }
                    }
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);
                    let bounds_len_op =
                        self.bounds_len_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op =
                        self.align_operand_for_ptr_local(tcx, body, dst, source_info.span);
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let args: Box<[Spanned<Operand<'tcx>>]> = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: parent_tag_op,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ]
                    .into_boxed_slice();

                    (args, Place::from(dst_tag))
                }
                _ => {
                    let is_mut_u8: u8 = match creation_kind {
                        InstrKind::Ref {
                            bk: borrow_kind, ..
                        } => match borrow_kind {
                            BorrowKind::Mut { .. } => 1,
                            _ => 0,
                        },
                        InstrKind::Raw { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        InstrKind::RetRoot { is_mut, .. } => {
                            if is_mut {
                                1
                            } else {
                                0
                            }
                        }
                        _ => 0,
                    };

                    let arg_mut = self.const_u8(tcx, source_info.span, is_mut_u8);

                    let arg_parent: Operand<'tcx> = match &creation_kind {
                        InstrKind::Ref { bk, src, .. } => {
                            let use_projectionless_anchor = matches!(bk, BorrowKind::Mut { .. })
                                || self.compile_alias_model_is_sb_like();
                            if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        false,
                                        use_projectionless_anchor,
                                    ),
                                )
                            }
                        }
                        InstrKind::Raw { src, .. } => {
                            let src_local = src.local;
                            let src_is_plain_ref_deref = src.projection.len() == 1
                                && matches!(src.projection[0], ProjectionElem::Deref)
                                && matches!(body.local_decls[src_local].ty.kind(), TyKind::Ref(..));
                            if src_is_plain_ref_deref {
                                if let Some(tl) = tag_local_for_ptr_local.get(&src_local) {
                                    Operand::Copy(Place::from(*tl))
                                } else if let Some(tl) =
                                    ref_ancestor_local_for_ptr_local.get(&src_local)
                                {
                                    Operand::Copy(Place::from(*tl))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                }
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    true,
                                    true,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx, body, *src, true, true,
                                    ),
                                )
                            }
                        }
                        _ => self.const_u64(tcx, source_info.span, 0),
                    };

                    let alias_exempt = match &creation_kind {
                        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
                            if let Some(dst_local) = place.as_local() {
                                let dst_ty = body.local_decls[dst_local].ty;
                                self.alias_exempt_for_ptr_ty(tcx, body, dst_ty)
                                    || self.alias_exempt_child_from_source_ptr(
                                        tcx,
                                        body,
                                        src.ty(&body.local_decls, tcx).ty,
                                        dst_ty,
                                    )
                            } else {
                                let ty = src.ty(&body.local_decls, tcx).ty;
                                self.alias_exempt_for_ty(tcx, body, ty)
                            }
                        }
                        InstrKind::RawRoot { ptr_local, .. } => {
                            let ty = body.local_decls[*ptr_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let ty = body.local_decls[*dst_local].ty;
                            self.alias_exempt_for_ptr_ty(tcx, body, ty)
                        }
                        _ => false,
                    };
                    // `alias_exempt` argument is a bitfield:
                    // - bit0: alias-exempt pointee classification (existing behavior)
                    // - bit1: projected-source creation hint (used by runtime lineage repair)
                    // - bit2: stronger root-origin repair hint (bounded overlap recovery)
                    // - bit4: TB-lite raw is a derived same-family view
                    // - bit6: raw creation should validate projected/derived provenance immediately
                    // - bit7: deref-based raw creation must reject exposed/no-provenance parents
                    let alias_flags: u8 = match &creation_kind {
                        InstrKind::Ref { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Even simple reference reborrows can lose their parent tag in
                            // optimized MIR/call-boundary lowering and show up as fresh roots at
                            // the same stack address. Always allow exact same-address repair for
                            // refs; keep the stronger bounded-overlap recovery limited to
                            // projection-heavy sources.
                            flags |= 0b10;
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                            }
                            let src_ty = src.ty(&body.local_decls, tcx).ty;
                            if self.is_pointer_ty(src_ty) && !self.is_thin_ptr_ty(tcx, body, src_ty)
                            {
                                // Wide-pointer reborrows often lower through a temporary thin raw
                                // data pointer. If that helper raw root loses lineage, the runtime
                                // should prefer dropping the bad raw-root parent over freezing the
                                // eventual wide ref/write as a foreign sibling.
                                flags |= 0b1_0000;
                            }
                            if src.projection.is_empty()
                                && projectionless_anchor_suppressed_locals.contains(&src.local)
                            {
                                // Whole-slot reborrows of by-value return carriers (`other:
                                // BytesMut; &mut other`) need targeted same-slot root retirement
                                // in TB-lite. Mark only this path so ordinary repeated `&mut`
                                // call arguments do not invalidate each other.
                                flags |= 0b1000;
                            }
                            flags
                        }
                        InstrKind::Raw { src, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Raw creation can still lose lineage when source-tag plumbing is
                            // missing (e.g. wrapper/projection-heavy optimized MIR). Mark all
                            // raw creations as eligible for runtime best-effort repair.
                            flags |= 0b10;
                            // Tree Borrows treats raw pointer creation as tag-preserving:
                            // casts, projections, and pointer arithmetic transport the source
                            // family rather than allocating a new permission-bearing borrow node.
                            // `InstrKind::Raw` is the structurally derived case; true fresh or
                            // unknown roots are emitted as `RawRoot`/`RetRoot` below. Mark every
                            // derived raw view so TB-lite keeps it as tag-store metadata until an
                            // actual raw write needs access-local raw state.
                            flags |= 0b0001_0000;
                            // For projected raw sources (`(*p).field`, etc.) also allow strong
                            // bounded-overlap parent recovery in the runtime repair path.
                            if !src.projection.is_empty() {
                                flags |= 0b100;
                                flags |= 0b0100_0000;
                            }
                            if matches!(src.projection.first(), Some(ProjectionElem::Deref))
                                && !self
                                    .raw_creation_allows_no_provenance_transport(tcx, body, *src)
                            {
                                flags |= 0b1000_0000;
                            }
                            flags
                        }
                        // RawRoot is emitted exactly in cases where provenance source recovery
                        // failed at instrumentation time. Mark for runtime best-effort repair.
                        InstrKind::RawRoot { .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            flags |= 0b10;
                            flags
                        }
                        InstrKind::RetRoot { dst_local, .. } => {
                            let mut flags = if alias_exempt { 1 } else { 0 };
                            // Return-root creation means caller-side provenance recovery failed.
                            // Mark both refs and raws as eligible for exact same-address repair:
                            // uninstrumented std/core pointer-returning wrappers can otherwise
                            // synthesize a fresh root for a pointer that should remain attached to
                            // an existing live lineage at the same address.
                            flags |= 0b10;
                            flags
                        }
                        _ => {
                            if alias_exempt {
                                1
                            } else {
                                0
                            }
                        }
                    };
                    let arg_alias = self.const_u8(tcx, source_info.span, alias_flags);

                    let bounds_ptr_local = match &creation_kind {
                        InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                        InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                        InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                        _ => None,
                    };
                    let bounds_len_op = bounds_ptr_local
                        .map(|pl| match &creation_kind {
                            InstrKind::Ref { .. } => self
                                .ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                ),
                            InstrKind::RetRoot { .. }
                                if matches!(body.local_decls[pl].ty.kind(), TyKind::Ref(..)) =>
                            {
                                self.ref_creation_bounds_len_operand_for_ptr_local(
                                    tcx,
                                    body,
                                    pl,
                                    source_info.span,
                                )
                            }
                            _ => self.bounds_len_operand_for_ptr_local(
                                tcx,
                                body,
                                pl,
                                source_info.span,
                            ),
                        })
                        .unwrap_or_else(|| self.unknown_bounds_len_operand(tcx, source_info.span));
                    let (arg_bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                    extra_stmts.append(&mut bounds_len_stmts);
                    let align_op = match &creation_kind {
                        InstrKind::Ref { src, .. } => self
                            .align_operand_for_ref_creation_src_place(
                                tcx,
                                body,
                                place.ty(&body.local_decls, tcx).ty,
                                *src,
                                source_info.span,
                            ),
                        InstrKind::Raw { src, .. } => {
                            self.align_operand_for_src_place(tcx, body, *src, source_info.span)
                        }
                        _ => bounds_ptr_local
                            .map(|pl| {
                                self.align_operand_for_ptr_local(tcx, body, pl, source_info.span)
                            })
                            .unwrap_or_else(|| {
                                SizeOperand::Const(self.const_usize(tcx, source_info.span, 0))
                            }),
                    };
                    let (arg_align, mut align_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);
                    extra_stmts.append(&mut align_stmts);

                    let mut explicit_extent: Option<(Operand<'tcx>, Operand<'tcx>)> = None;
                    if let InstrKind::Ref { bk, src, .. } = &creation_kind {
                        if !matches!(bk, BorrowKind::Mut { .. }) {
                            if let Some((arg_extent_base, extent_len_op, mut extent_stmts)) = self
                                .interior_mut_array_extent_for_source_place(
                                    tcx,
                                    body,
                                    source_info,
                                    *src,
                                )
                            {
                                extra_stmts.append(&mut extent_stmts);
                                let (arg_extent_len, mut extent_len_stmts) = self
                                    .materialize_size_operand(
                                        tcx,
                                        body,
                                        source_info,
                                        &extent_len_op,
                                    );
                                extra_stmts.append(&mut extent_len_stmts);
                                func_operand = Operand::function_handle(
                                    tcx,
                                    hooks.def_id_ref_with_extent,
                                    std::iter::empty(),
                                    source_info.span,
                                );
                                explicit_extent = Some((arg_extent_base, arg_extent_len));
                            }
                        }
                    }

                    let mut args = vec![
                        Spanned {
                            node: arg_addr,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_mut,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_parent,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_alias,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_bounds_len,
                            span: source_info.span,
                        },
                        Spanned {
                            node: arg_align,
                            span: source_info.span,
                        },
                    ];
                    if let Some((arg_extent_base, arg_extent_len)) = explicit_extent {
                        args.push(Spanned {
                            node: arg_extent_base,
                            span: source_info.span,
                        });
                        args.push(Spanned {
                            node: arg_extent_len,
                            span: source_info.span,
                        });
                    }
                    let args: Box<[Spanned<Operand<'tcx>>]> = args.into_boxed_slice();

                    let dst = tag_local.expect("missing tag_local for ref/raw creation");
                    (args, Place::from(dst))
                }
            };

            let mut call_term = Terminator {
                source_info,
                kind: TerminatorKind::Call {
                    func: func_operand,
                    args,
                    destination: dest_place,
                    target: Some(cont_block),
                    unwind: UnwindAction::Continue,
                    call_source: CallSource::Misc,
                    fn_span: source_info.span,
                },
            };

            if let Some((ptr_local, true)) = heap_alloc_info {
                let raw_bb = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(None, is_cleanup));

                if let TerminatorKind::Call { target, .. } = &mut call_term.kind {
                    *target = Some(raw_bb);
                }

                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for heap alloc ptr");

                let raw_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_raw,
                    std::iter::empty(),
                    source_info.span,
                );
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);

                let raw_args: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, 1),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u64(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(
                            tcx,
                            source_info.span,
                            if alias_exempt { 1 } else { 0 },
                        ),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_usize(tcx, source_info.span, 0),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                body.basic_blocks_mut()[raw_bb].terminator = Some(Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: raw_func,
                        args: raw_args,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                });
            }

            let ref_ancestor_init_stmt_opt: Option<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let use_projectionless_anchor = match &creation_kind {
                                InstrKind::Ref { bk, .. } => {
                                    matches!(bk, BorrowKind::Mut { .. })
                                        || self.compile_alias_model_is_sb_like()
                                }
                                _ => true,
                            };
                            let parent_op = if let Some(local) =
                                projected_ref_parent_local.or(projectionless_ref_parent_local)
                            {
                                Operand::Copy(Place::from(local))
                            } else {
                                self.parent_tag_operand_for_src_place(
                                    tcx,
                                    body,
                                    bb,
                                    stmt_idx,
                                    source_info,
                                    *src,
                                    tag_local_for_ptr_local,
                                    ref_ancestor_local_for_ptr_local,
                                    reborrow_anchor_local_for_stack_local,
                                    projectionless_anchor_suppressed_locals,
                                    false,
                                    use_projectionless_anchor,
                                    self.creation_parent_selection_mode_for_src_place(
                                        tcx,
                                        body,
                                        *src,
                                        false,
                                        use_projectionless_anchor,
                                    ),
                                )
                            };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(parent_op),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if let Some(dst_ref_ancestor_local) =
                            ref_ancestor_local_for_ptr_local.get(&dst_local).copied()
                        {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(&src.local)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                InstrKind::RawRoot { ptr_local, .. } => ref_ancestor_local_for_ptr_local
                    .get(ptr_local)
                    .copied()
                    .map(|dst_ref_ancestor_local| {
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(dst_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        )
                    }),
                InstrKind::RetRoot {
                    dst_local, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst_local).copied()
                    {
                        if *is_ref {
                            tag_local_for_ptr_local
                                .get(dst_local)
                                .copied()
                                .map(|dst_tag_local| {
                                    Statement::new(
                                        source_info,
                                        StatementKind::Assign(Box::new((
                                            Place::from(dst_ref_ancestor_local),
                                            Rvalue::Use(Operand::Copy(Place::from(dst_tag_local))),
                                        ))),
                                    )
                                })
                        } else {
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                InstrKind::PtrDerive {
                    dst, src, is_ref, ..
                } => {
                    if let Some(dst_ref_ancestor_local) =
                        ref_ancestor_local_for_ptr_local.get(dst).copied()
                    {
                        if *is_ref {
                            let src_parent_op: Operand<'tcx> =
                                if let Some(src_tag_local) = tag_local_for_ptr_local.get(src) {
                                    Operand::Copy(Place::from(*src_tag_local))
                                } else if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(src)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_parent_op),
                                ))),
                            ))
                        } else {
                            let src_ref_ancestor_op: Operand<'tcx> =
                                if let Some(src_ref_ancestor_local) =
                                    ref_ancestor_local_for_ptr_local.get(src)
                                {
                                    Operand::Copy(Place::from(*src_ref_ancestor_local))
                                } else {
                                    self.const_u64(tcx, source_info.span, 0)
                                };
                            Some(Statement::new(
                                source_info,
                                StatementKind::Assign(Box::new((
                                    Place::from(dst_ref_ancestor_local),
                                    Rvalue::Use(src_ref_ancestor_op),
                                ))),
                            ))
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let export_parent_init_stmts: Vec<Statement<'tcx>> = {
                let direct_dst_local = match &creation_kind {
                    InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
                    InstrKind::RawRoot { ptr_local, .. } => Some(*ptr_local),
                    InstrKind::RetRoot { dst_local, .. } => Some(*dst_local),
                    InstrKind::PtrDerive { dst, .. } | InstrKind::PtrDeriveParent { dst, .. } => {
                        Some(*dst)
                    }
                    InstrKind::DebugRefActivate { raw_local, .. } => Some(*raw_local),
                    _ => None,
                };
                if let Some(dst_local) = direct_dst_local {
                    let mut stmts = Vec::new();
                    let mut recovered_init_local: Option<Local> = None;
                    if let (Some(export_parent_local), Some(dst_tag_local)) = (
                        export_parent_local_for_ptr_local.get(&dst_local).copied(),
                        tag_local_for_ptr_local.get(&dst_local).copied(),
                    ) {
                        let default_export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref {
                                bk,
                                src,
                                projected_reborrow_anchor_key,
                                ..
                            } if matches!(bk, BorrowKind::Mut { .. })
                                && matches!(
                                    body.local_decls[dst_local].ty.kind(),
                                    TyKind::Ref(_, pointee_ty, Mutability::Mut)
                                        if !self.is_pointer_ty(*pointee_ty)
                                ) =>
                            {
                                projected_reborrow_anchor_key
                                    .as_ref()
                                    .and_then(|key| {
                                        projected_reborrow_anchor_local_for_key.get(key).copied()
                                    })
                                    .or_else(|| {
                                        if src.projection.is_empty() {
                                            reborrow_anchor_local_for_stack_local
                                                .get(&src.local)
                                                .copied()
                                        } else {
                                            None
                                        }
                                    })
                                    .map(|anchor_local| Operand::Copy(Place::from(anchor_local)))
                                    .unwrap_or_else(|| Operand::Copy(Place::from(dst_tag_local)))
                            }
                            _ => Operand::Copy(Place::from(dst_tag_local)),
                        };
                        let export_parent_op: Operand<'tcx> = match &creation_kind {
                            InstrKind::Ref { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::Raw { src, .. } => {
                                let src_local = src.local;
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(&src_local).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(&src_local)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            InstrKind::PtrDerive {
                                src, is_ref: true, ..
                            } => {
                                if let (Some(src_export_parent_local), Some(src_recovered_local)) = (
                                    export_parent_local_for_ptr_local.get(src).copied(),
                                    export_parent_is_recovered_local_for_ptr_local
                                        .get(src)
                                        .copied(),
                                ) {
                                    recovered_init_local = Some(src_recovered_local);
                                    let recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let not_recovered_u64_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let fallback_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let export_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let selected_tag_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    stmts.extend([
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(recovered_u64_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(src_recovered_local)),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(not_recovered_u64_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Sub,
                                                    Box::new((
                                                        self.const_u64(tcx, source_info.span, 1),
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(fallback_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            not_recovered_u64_local,
                                                        )),
                                                        default_export_parent_op.clone(),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(export_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            recovered_u64_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            src_export_parent_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(selected_tag_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            fallback_part_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            export_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                    ]);

                                    Operand::Copy(Place::from(selected_tag_local))
                                } else {
                                    default_export_parent_op
                                }
                            }
                            _ => default_export_parent_op,
                        };
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_op),
                            ))),
                        ));
                    }
                    if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                        .get(&dst_local)
                        .copied()
                    {
                        stmts.push(Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(
                                    recovered_init_local
                                        .map(|local| Operand::Copy(Place::from(local)))
                                        .unwrap_or_else(|| self.const_u8(tcx, source_info.span, 0)),
                                ),
                            ))),
                        ));
                    }
                    stmts
                } else {
                    Vec::new()
                }
            };
            let reborrow_anchor_set_stmts: Vec<Statement<'tcx>> = match &creation_kind {
                InstrKind::Ref {
                    bk,
                    src,
                    projected_reborrow_anchor_key,
                    ..
                } => {
                    if matches!(bk, BorrowKind::Mut { .. }) {
                        if let Some(dst_local) = place.as_local() {
                            let anchor_local_opt = projected_reborrow_anchor_key
                                .as_ref()
                                .and_then(|key| projected_reborrow_anchor_local_for_key.get(key))
                                .copied()
                                .or_else(|| {
                                    if src.projection.is_empty() {
                                        reborrow_anchor_local_for_stack_local
                                            .get(&src.local)
                                            .copied()
                                    } else {
                                        None
                                    }
                                });
                            let anchor_state_local_opt = if src.projection.is_empty() {
                                anchor_is_slot_family_local_for_stack_local
                                    .get(&src.local)
                                    .copied()
                            } else {
                                None
                            };
                            if let Some(anchor_local) = anchor_local_opt {
                                if let Some(anchor_source_local) = projected_ref_parent_local
                                    .or_else(|| tag_local_for_ptr_local.get(&dst_local).copied())
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    let mut stmts = vec![
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_is_zero_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Eq,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        self.const_u64(tcx, source_info.span, 0),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ];
                                    if let Some(anchor_state_local) = anchor_state_local_opt {
                                        stmts.push(Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_state_local),
                                                Rvalue::Use(self.const_u8(
                                                    tcx,
                                                    source_info.span,
                                                    1,
                                                )),
                                            ))),
                                        ));
                                    }
                                    stmts
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                InstrKind::Raw { src, .. } => {
                    if let Some(dst_local) = place.as_local() {
                        if src.projection.is_empty() {
                            if let Some(anchor_local) = reborrow_anchor_local_for_stack_local
                                .get(&src.local)
                                .copied()
                            {
                                if let Some(anchor_source_local) =
                                    tag_local_for_ptr_local.get(&dst_local).copied()
                                {
                                    let anchor_is_zero_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                                    let anchor_should_init_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_new_part_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                                    let anchor_selected_local = body
                                        .local_decls
                                        .push(LocalDecl::new(tcx.types.u64, source_info.span));

                                    vec![
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_is_zero_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Eq,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        self.const_u64(tcx, source_info.span, 0),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_should_init_local),
                                                Rvalue::Cast(
                                                    CastKind::IntToInt,
                                                    Operand::Copy(Place::from(
                                                        anchor_is_zero_local,
                                                    )),
                                                    tcx.types.u64,
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_new_part_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Mul,
                                                    Box::new((
                                                        Operand::Copy(Place::from(
                                                            anchor_should_init_local,
                                                        )),
                                                        Operand::Copy(Place::from(
                                                            anchor_source_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_selected_local),
                                                Rvalue::BinaryOp(
                                                    BinOp::Add,
                                                    Box::new((
                                                        Operand::Copy(Place::from(anchor_local)),
                                                        Operand::Copy(Place::from(
                                                            anchor_new_part_local,
                                                        )),
                                                    )),
                                                ),
                                            ))),
                                        ),
                                        Statement::new(
                                            source_info,
                                            StatementKind::Assign(Box::new((
                                                Place::from(anchor_local),
                                                Rvalue::Use(Operand::Copy(Place::from(
                                                    anchor_selected_local,
                                                ))),
                                            ))),
                                        ),
                                    ]
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };

            let split_at = {
                let len = body.basic_blocks[bb].statements.len();
                if stmt_idx >= len {
                    len
                } else if insert_before {
                    stmt_idx
                } else {
                    stmt_idx + 1
                }
            };
            let split_at_prefix = split_at.min(orig_stmt_len);
            orig_stmt_prefix_len.insert(bb, split_at_prefix);
            orig_stmt_prefix_len.insert(cont_block, orig_stmt_len.saturating_sub(split_at_prefix));

            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];

                let rem = bd.statements.split_off(split_at);

                if let Some(s1) = addr_stmt1_opt {
                    bd.statements.push(s1);
                }
                bd.statements.push(addr_stmt2);
                if let Some(s3) = addr_adjust_stmt_opt {
                    bd.statements.push(s3);
                }
                if !extra_stmts.is_empty() {
                    bd.statements.extend(extra_stmts);
                }

                bd.terminator = Some(call_term);
                rem
            };

            if !reborrow_anchor_set_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, reborrow_anchor_set_stmts);
            }
            if !export_parent_init_stmts.is_empty() {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .splice(0..0, export_parent_init_stmts);
            }
            if let Some(ref_ancestor_init_stmt) = ref_ancestor_init_stmt_opt {
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .insert(0, ref_ancestor_init_stmt);
            }
            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
        }

        // Insert ArgRetag points after all other instrumentation.
        // Because each insertion rewrites the entry terminator, the last-applied
        // retag runs first at runtime, ensuring tags are initialized before reads.
        for (_idx, ip) in arg_retag_points.into_iter().rev() {
            let bb = ip.bb;
            let stmt_idx = ip.stmt_idx;
            let source_info = ip.source_info;
            let place = ip.place;
            let creation_kind = ip.kind;

            if let InstrKind::FnEnter { callee_id } = creation_kind {
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let enter_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_enter_fn,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![Spanned {
                            node: self.const_u64(tcx, source_info.span, callee_id),
                            span: source_info.span,
                        }]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = stmt_idx.min(bd.statements.len());
                    let rem = bd.statements.split_off(split_at);
                    bd.terminator = Some(enter_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorSeedFromShadow { local } = creation_kind {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorSeedFromShadow");
                let anchor_state_local = anchor_is_slot_family_local_for_stack_local
                    .get(&local)
                    .copied();
                let seed_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((addr_stmt1, addr_stmt2)) = self.slot_addr_stmts_for_place(
                    tcx,
                    body,
                    source_info,
                    place,
                    seed_addr_local,
                    false,
                ) else {
                    continue;
                };
                let seed_ptr_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let Some((seed_ptr_addr_stmt1_opt, seed_ptr_addr_stmt2)) =
                    self.addr_stmts_for_place(tcx, body, source_info, place, seed_ptr_addr_local)
                else {
                    continue;
                };
                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = body
                    .basic_blocks_mut()
                    .push(BasicBlockData::new(orig_term, is_cleanup));
                let load_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_shadow_load_tag_for_ptr,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: Operand::Copy(Place::from(seed_addr_local)),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(seed_ptr_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(anchor_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    if let Some(seed_ptr_addr_stmt1) = seed_ptr_addr_stmt1_opt {
                        bd.statements.push(seed_ptr_addr_stmt1);
                    }
                    bd.statements.push(seed_ptr_addr_stmt2);
                    bd.terminator = Some(load_term);
                    rem
                };
                if let Some(anchor_state_local) = anchor_state_local {
                    let anchor_nonzero_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.bool, source_info.span));
                    let anchor_state_u8_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u8, source_info.span));
                    body.basic_blocks_mut()[cont_block].statements.extend([
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_nonzero_local),
                                Rvalue::BinaryOp(
                                    BinOp::Ne,
                                    Box::new((
                                        Operand::Copy(Place::from(anchor_local)),
                                        self.const_u64(tcx, source_info.span, 0),
                                    )),
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_u8_local),
                                Rvalue::Cast(
                                    CastKind::IntToInt,
                                    Operand::Copy(Place::from(anchor_nonzero_local)),
                                    tcx.types.u8,
                                ),
                            ))),
                        ),
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(anchor_state_local),
                                Rvalue::Use(Operand::Copy(Place::from(anchor_state_u8_local))),
                            ))),
                        ),
                    ]);
                }
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgRetag {
                callee_id,
                arg_index,
                ptr_local,
            } = creation_kind
            {
                let tag_local = *tag_local_for_ptr_local
                    .get(&ptr_local)
                    .expect("missing tag local for ArgRetag");

                // Take the caller-pushed tag first, then create a fresh tag for this argument.
                let (record_def_id, is_mut_u8) = match body.local_decls[ptr_local].ty.kind() {
                    TyKind::Ref(_, _ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_ref, if is_mut { 1 } else { 0 })
                    }
                    TyKind::RawPtr(_ty, mutbl) => {
                        let is_mut = matches!(mutbl, Mutability::Mut);
                        (hooks.def_id_raw, if is_mut { 1 } else { 0 })
                    }
                    _ => panic!("ArgRetag on non-pointer local"),
                };
                let alias_exempt =
                    self.alias_exempt_for_ptr_ty(tcx, body, body.local_decls[ptr_local].ty);
                let alias_flags = {
                    let mut flags = if alias_exempt { 1 } else { 0 };
                    if matches!(body.local_decls[ptr_local].ty.kind(), TyKind::Ref(..)) {
                        flags |= 0b10;
                    }
                    flags
                };

                // Retagging uses the data pointer for wide pointers so derived raw pointers share the tag.
                let addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let parent_tag_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.u64, source_info.span));

                let (addr_stmt1_opt, addr_stmt2) = self
                    .addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        Place::from(ptr_local),
                        addr_local,
                    )
                    .expect("ArgRetag on non-pointer local");

                let ptr_ty = body.local_decls[ptr_local].ty;
                let bounds_len_op = if matches!(ptr_ty.kind(), TyKind::Ref(..)) {
                    self.ref_creation_bounds_len_operand_for_ptr_local(
                        tcx,
                        body,
                        ptr_local,
                        source_info.span,
                    )
                } else {
                    self.bounds_len_operand_for_ptr_local(tcx, body, ptr_local, source_info.span)
                };
                let (arg_bounds_len, mut bounds_len_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &bounds_len_op);
                let align_op =
                    self.align_operand_for_ptr_local(tcx, body, ptr_local, source_info.span);
                let (arg_align, mut align_stmts) =
                    self.materialize_size_operand(tcx, body, source_info, &align_op);

                let arg_callee = self.const_u64(tcx, source_info.span, callee_id);
                let arg_index = self.const_u64(tcx, source_info.span, arg_index);
                let arg_addr = Operand::Copy(Place::from(addr_local));

                let args_take: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: arg_callee,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_index,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_addr,
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let args_record: Box<[Spanned<Operand<'tcx>>]> = vec![
                    Spanned {
                        node: Operand::Copy(Place::from(addr_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, is_mut_u8),
                        span: source_info.span,
                    },
                    Spanned {
                        node: Operand::Copy(Place::from(parent_tag_local)),
                        span: source_info.span,
                    },
                    Spanned {
                        node: self.const_u8(tcx, source_info.span, alias_flags),
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_bounds_len,
                        span: source_info.span,
                    },
                    Spanned {
                        node: arg_align,
                        span: source_info.span,
                    },
                ]
                .into_boxed_slice();

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };

                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };

                let record_func = Operand::function_handle(
                    tcx,
                    record_def_id,
                    std::iter::empty(),
                    source_info.span,
                );
                let record_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: record_func,
                        args: args_record,
                        destination: Place::from(tag_local),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let retag_block = {
                    let retag_data = BasicBlockData::new(Some(record_term), is_cleanup);
                    body.basic_blocks_mut().push(retag_data)
                };

                let take_func = Operand::function_handle(
                    tcx,
                    hooks.def_id_take_call_arg_tag,
                    std::iter::empty(),
                    source_info.span,
                );
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: take_func,
                        args: args_take,
                        destination: Place::from(parent_tag_local),
                        target: Some(retag_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };

                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    if let Some(addr_stmt1) = addr_stmt1_opt {
                        bd.statements.push(addr_stmt1);
                    }
                    bd.statements.push(addr_stmt2);
                    if !bounds_len_stmts.is_empty() {
                        bd.statements.append(&mut bounds_len_stmts);
                    }
                    if !align_stmts.is_empty() {
                        bd.statements.append(&mut align_stmts);
                    }
                    bd.terminator = Some(take_term);
                    rem
                };

                if let Some(arg_ref_ancestor_local) =
                    ref_ancestor_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let ref_ancestor_stmt = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(tag_local))),
                            ))),
                        ),
                        TyKind::RawPtr(..) => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(Operand::Copy(Place::from(parent_tag_local))),
                            ))),
                        ),
                        _ => Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(arg_ref_ancestor_local),
                                Rvalue::Use(self.const_u64(tcx, source_info.span, 0)),
                            ))),
                        ),
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .insert(0, ref_ancestor_stmt);
                }
                if let Some(export_parent_local) =
                    export_parent_local_for_ptr_local.get(&ptr_local).copied()
                {
                    let export_parent_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => {
                            Operand::Copy(Place::from(parent_tag_local))
                        }
                        _ => Operand::Copy(Place::from(tag_local)),
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(export_parent_local),
                                Rvalue::Use(export_parent_value),
                            ))),
                        ),
                    );
                }
                if let Some(recovered_local) = export_parent_is_recovered_local_for_ptr_local
                    .get(&ptr_local)
                    .copied()
                {
                    let recovered_value = match body.local_decls[ptr_local].ty.kind() {
                        TyKind::Ref(_, _, Mutability::Not) => 1,
                        _ => 0,
                    };
                    body.basic_blocks_mut()[cont_block].statements.insert(
                        0,
                        Statement::new(
                            source_info,
                            StatementKind::Assign(Box::new((
                                Place::from(recovered_local),
                                Rvalue::Use(self.const_u8(tcx, source_info.span, recovered_value)),
                            ))),
                        ),
                    );
                }

                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
                continue;
            }

            if let InstrKind::ArgAnchorTake {
                callee_id,
                arg_index,
                local,
            } = creation_kind
            {
                let anchor_local = *reborrow_anchor_local_for_stack_local
                    .get(&local)
                    .expect("missing anchor local for ArgAnchorTake");
                let local_ty = body.local_decls[local].ty;
                if self.is_box_ty(tcx, local_ty) {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (take_addr_stmt1, take_addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let pointee_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let Some((pointee_addr_stmt1, pointee_addr_stmt2)) = self
                        .box_pointee_slot_addr_stmts_for_local(
                            tcx,
                            body,
                            source_info,
                            local,
                            pointee_addr_local,
                        )
                    else {
                        panic!("ArgAnchorTake box local missing pointee address support");
                    };

                    let (orig_term, is_cleanup) = {
                        let bd = &mut body.basic_blocks_mut()[bb];
                        let term = bd.terminator.take();
                        let cleanup = bd.is_cleanup;
                        (term, cleanup)
                    };
                    let cont_block = {
                        let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                        body.basic_blocks_mut().push(cont_data)
                    };
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(pointee_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 1),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_usize(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let remaining_stmts = {
                        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                        let split_at = if stmt_idx > bd.statements.len() {
                            bd.statements.len()
                        } else {
                            stmt_idx
                        };
                        let rem = bd.statements.split_off(split_at);
                        bd.statements.push(take_addr_stmt1);
                        bd.statements.push(take_addr_stmt2);
                        bd.statements.push(pointee_addr_stmt1);
                        bd.statements.push(pointee_addr_stmt2);
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                } else {
                    let take_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (addr_stmt1, addr_stmt2) = self
                        .slot_addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            Place::from(local),
                            take_addr_local,
                            false,
                        )
                        .expect("ArgAnchorTake on unsupported local");
                    let Some((field_place, pointee_ty, is_mut)) =
                        self.first_direct_ref_field_place(tcx, body, local)
                    else {
                        // By-value call-boundary anchors are only valid for aggregates that
                        // structurally carry a source-level reference. If we cannot recover such a
                        // field, skip the synthetic root ref instead of inventing one for the
                        // whole slot.
                        continue;
                    };
                    let record_addr_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.usize, source_info.span));
                    let (record_addr_stmt1_opt, record_addr_stmt2) = self
                        .addr_stmts_for_place(
                            tcx,
                            body,
                            source_info,
                            field_place,
                            record_addr_local,
                        )
                        .expect("ArgAnchorTake direct ref field address");
                    let record_is_mut = is_mut;
                    let record_ty = pointee_ty;
                    let parent_tag_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.u64, source_info.span));
                    let size_op = self.size_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (bounds_len, mut bounds_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &size_op);
                    let align_op =
                        self.align_operand_for_ty(tcx, body, record_ty, source_info.span);
                    let (align_len, mut align_len_stmts) =
                        self.materialize_size_operand(tcx, body, source_info, &align_op);

                    let (orig_term, is_cleanup) = {
                        let bd = &mut body.basic_blocks_mut()[bb];
                        let term = bd.terminator.take();
                        let cleanup = bd.is_cleanup;
                        (term, cleanup)
                    };
                    let cont_block = {
                        let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                        body.basic_blocks_mut().push(cont_data)
                    };
                    let record_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_ref,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: Operand::Copy(Place::from(record_addr_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(
                                        tcx,
                                        source_info.span,
                                        if record_is_mut { 1 } else { 0 },
                                    ),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(parent_tag_local)),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u8(tcx, source_info.span, 0),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: bounds_len,
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: align_len,
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(anchor_local),
                            target: Some(cont_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let record_block = {
                        let record_data = BasicBlockData::new(Some(record_term), is_cleanup);
                        body.basic_blocks_mut().push(record_data)
                    };
                    let take_term = Terminator {
                        source_info,
                        kind: TerminatorKind::Call {
                            func: Operand::function_handle(
                                tcx,
                                hooks.def_id_take_call_arg_tag_anchor,
                                std::iter::empty(),
                                source_info.span,
                            ),
                            args: vec![
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, callee_id),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: self.const_u64(tcx, source_info.span, arg_index),
                                    span: source_info.span,
                                },
                                Spanned {
                                    node: Operand::Copy(Place::from(take_addr_local)),
                                    span: source_info.span,
                                },
                            ]
                            .into_boxed_slice(),
                            destination: Place::from(parent_tag_local),
                            target: Some(record_block),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: source_info.span,
                        },
                    };
                    let remaining_stmts = {
                        let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                        let split_at = if stmt_idx > bd.statements.len() {
                            bd.statements.len()
                        } else {
                            stmt_idx
                        };
                        let rem = bd.statements.split_off(split_at);
                        bd.statements.push(addr_stmt1);
                        bd.statements.push(addr_stmt2);
                        if let Some(record_addr_stmt1) = record_addr_stmt1_opt {
                            bd.statements.push(record_addr_stmt1);
                        }
                        bd.statements.push(record_addr_stmt2);
                        if !bounds_len_stmts.is_empty() {
                            bd.statements.append(&mut bounds_len_stmts);
                        }
                        if !align_len_stmts.is_empty() {
                            bd.statements.append(&mut align_len_stmts);
                        }
                        bd.terminator = Some(take_term);
                        rem
                    };
                    body.basic_blocks_mut()[cont_block]
                        .statements
                        .extend(remaining_stmts);
                }
            }

            if let InstrKind::ArgLeafTake {
                callee_id,
                arg_index,
                leaf_key,
            } = creation_kind
            {
                let take_addr_local = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.usize, source_info.span));
                let (addr_stmt1, addr_stmt2) = self
                    .slot_addr_stmts_for_place(
                        tcx,
                        body,
                        source_info,
                        place,
                        take_addr_local,
                        false,
                    )
                    .expect("ArgLeafTake on unsupported local leaf slot");
                let tmp_unit = body
                    .local_decls
                    .push(LocalDecl::new(tcx.types.unit, source_info.span));

                let (orig_term, is_cleanup) = {
                    let bd = &mut body.basic_blocks_mut()[bb];
                    let term = bd.terminator.take();
                    let cleanup = bd.is_cleanup;
                    (term, cleanup)
                };
                let cont_block = {
                    let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                    body.basic_blocks_mut().push(cont_data)
                };
                let take_term = Terminator {
                    source_info,
                    kind: TerminatorKind::Call {
                        func: Operand::function_handle(
                            tcx,
                            hooks.def_id_take_call_arg_leaf_shadow,
                            std::iter::empty(),
                            source_info.span,
                        ),
                        args: vec![
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, callee_id),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, arg_index),
                                span: source_info.span,
                            },
                            Spanned {
                                node: self.const_u64(tcx, source_info.span, leaf_key),
                                span: source_info.span,
                            },
                            Spanned {
                                node: Operand::Copy(Place::from(take_addr_local)),
                                span: source_info.span,
                            },
                        ]
                        .into_boxed_slice(),
                        destination: Place::from(tmp_unit),
                        target: Some(cont_block),
                        unwind: UnwindAction::Continue,
                        call_source: CallSource::Misc,
                        fn_span: source_info.span,
                    },
                };
                let remaining_stmts = {
                    let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                    let split_at = if stmt_idx > bd.statements.len() {
                        bd.statements.len()
                    } else {
                        stmt_idx
                    };
                    let rem = bd.statements.split_off(split_at);
                    bd.statements.push(addr_stmt1);
                    bd.statements.push(addr_stmt2);
                    bd.terminator = Some(take_term);
                    rem
                };
                body.basic_blocks_mut()[cont_block]
                    .statements
                    .extend(remaining_stmts);
            }
        }
    }

    fn insert_fallback_return_stack_allocs<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &mut Body<'tcx>,
        fallback_return_locals: &[(Local, SizeOperand<'tcx>)],
        manual_holder_managed_tag_assignments: &mut HashSet<(BasicBlock, usize)>,
        tag_local_for_ptr_local: &HashMap<Local, Local>,
        local_slot_shadow_store_locals: &HashSet<Local>,
        export_parent_local_for_ptr_local: &HashMap<Local, Local>,
        export_parent_is_recovered_local_for_ptr_local: &HashMap<Local, Local>,
        ref_ancestor_local_for_ptr_local: &HashMap<Local, Local>,
        reborrow_anchor_local_for_stack_local: &HashMap<Local, Local>,
        anchor_is_slot_family_local_for_stack_local: &HashMap<Local, Local>,
        projectionless_anchor_suppressed_locals: &HashSet<Local>,
        projected_reborrow_anchor_local_for_key: &HashMap<String, Local>,
        debug_ref_bindings: &HashMap<DebugRefBindingKey, DebugRefBinding>,
        hooks: Hooks,
    ) {
        if fallback_return_locals.is_empty() {
            return;
        }

        let mut insert_points: Vec<InsertPoint<'tcx>> = Vec::new();
        for (bb, block_data) in body.basic_blocks.iter_enumerated() {
            let Some(term) = block_data.terminator.as_ref() else {
                continue;
            };
            if !matches!(term.kind, TerminatorKind::Return) {
                continue;
            }

            for (local, size_op) in fallback_return_locals.iter().cloned() {
                insert_points.push(InsertPoint {
                    bb,
                    stmt_idx: block_data.statements.len(),
                    insert_before: false,
                    source_info: term.source_info,
                    place: Place::from(local),
                    kind: InstrKind::StackAlloc {
                        local,
                        live: false,
                        size_op: size_op.clone(),
                    },
                });
                self.trace_stack_alloc_emit(tcx, body, local, false, &size_op, "FallbackReturn");
            }
        }

        self.insert_instrumentation(
            tcx,
            body,
            insert_points,
            manual_holder_managed_tag_assignments,
            tag_local_for_ptr_local,
            local_slot_shadow_store_locals,
            export_parent_local_for_ptr_local,
            export_parent_is_recovered_local_for_ptr_local,
            ref_ancestor_local_for_ptr_local,
            reborrow_anchor_local_for_stack_local,
            anchor_is_slot_family_local_for_stack_local,
            projectionless_anchor_suppressed_locals,
            projected_reborrow_anchor_local_for_key,
            debug_ref_bindings,
            hooks,
        );
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
                        kind: InstrKind::TagLocalKill {
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
