//! Filters instrumentation using unsafe-dataflow results and reports the outcome.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::unsafe_dataflow::{UnsafeInfluence, UnsafeSummaryRecord};

use super::*;

#[derive(Default)]
struct UnsafeDflowStats {
    functions_seen: usize,
    functions_enabled: usize,
    ptr_locals_tainted_total: usize,
    ptr_locals_total: usize,
    hooks_total_before: usize,
    hooks_total_after: usize,
    access_hooks_before: usize,
    access_hooks_after: usize,
}

#[derive(Default)]
struct UnsafeCallDflowStats {
    seed_arg_unknown_boundary: usize,
    seed_arg_local_summary_missing: usize,
    seed_arg_summary_direct_sink: usize,
    seed_arg_summary_escape_unknown_direct: usize,
    seed_arg_summary_escape_unknown_inherited: usize,
    seed_arg_raw_fallback: usize,
    backward_dst_unknown_boundary: usize,
    backward_dst_local_summary_missing: usize,
    backward_dst_forward_to_return: usize,
    unknown_callees: std::collections::BTreeMap<String, (usize, usize)>,
}

#[derive(Default)]
struct UnsafeSummaryStats {
    functions_seen: usize,
    functions_with_direct_sink: usize,
    functions_calling_unknown_boundary: usize,
    functions_calling_unknown_boundary_direct: usize,
    functions_calling_unknown_boundary_inherited: usize,
    ptr_args_total: usize,
    ptr_args_with_direct_sink: usize,
    ptr_args_escaping_unknown: usize,
    ptr_args_escaping_unknown_direct: usize,
    ptr_args_escaping_unknown_inherited: usize,
    ptr_args_forwarded_to_return: usize,
}

impl MyOptimizationPass {
    /// Pre-scan the body to find pointer locals that are assigned from a known pointer source.
    /// This prevents later RawRoot insertion from overwriting tags when control-flow order
    /// differs from basic-block index order.
    pub(in crate::instrumentation) fn collect_ptr_locals_with_tag_sources<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
    ) -> HashSet<Local> {
        let mut locals = HashSet::new();

        for block_data in body.basic_blocks.iter() {
            for stmt in block_data.statements.iter() {
                let StatementKind::Assign(box (dst_place, rvalue)) = &stmt.kind else {
                    continue;
                };
                let Some(dst_local) = dst_place.as_local() else {
                    continue;
                };
                let dst_ty = body.local_decls[dst_local].ty;
                if !self.is_pointer_ty(dst_ty) {
                    continue;
                }

                match rvalue {
                    Rvalue::Ref(..) | Rvalue::RawPtr(..) => {
                        locals.insert(dst_local);
                    }
                    Rvalue::Use(op) => {
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    Rvalue::CopyForDeref(p) => {
                        if let Some(src_local) = p.as_local() {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    Rvalue::Cast(
                        CastKind::PtrToPtr | CastKind::PointerCoercion(_, _) | CastKind::Transmute,
                        op,
                        _,
                    ) => {
                        let mut has_tag_source = false;
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) {
                                has_tag_source = true;
                            }
                        }
                        if !has_tag_source
                            && matches!(rvalue, Rvalue::Cast(CastKind::Transmute, ..))
                        {
                            let src_ty = op.ty(body, tcx);
                            has_tag_source = match src_ty.kind() {
                                TyKind::Adt(adt, _) => {
                                    let name = tcx.def_path_str(adt.did());
                                    name.contains("::NonNull") || name.contains("::Unique")
                                }
                                _ => false,
                            };
                        }
                        if has_tag_source {
                            locals.insert(dst_local);
                        }
                    }
                    Rvalue::Cast(CastKind::PointerWithExposedProvenance, op, _) => {
                        if let Some(src_local) =
                            self.place_from_operand(op).and_then(|p| p.as_local())
                        {
                            let src_ty = body.local_decls[src_local].ty;
                            if self.is_pointer_ty(src_ty) || src_ty.is_integral() {
                                locals.insert(dst_local);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        locals
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_gated_local<'tcx>(
        kind: &InstrKind<'tcx>,
        place: &Place<'tcx>,
    ) -> Option<Local> {
        match kind {
            InstrKind::PtrRead { ptr_local, .. }
            | InstrKind::PtrWrite { ptr_local, .. }
            | InstrKind::PtrReadAllowUntagged { ptr_local, .. }
            | InstrKind::PtrWriteAllowUntagged { ptr_local, .. }
            | InstrKind::PtrUse { ptr_local }
            | InstrKind::RawRoot { ptr_local, .. }
            | InstrKind::RetRoot {
                dst_local: ptr_local,
                ..
            }
            | InstrKind::PtrDerive { dst: ptr_local, .. } => Some(*ptr_local),
            InstrKind::Ref { .. } | InstrKind::Raw { .. } => place.as_local(),
            _ => None,
        }
    }

    pub(in crate::instrumentation) fn log_unsafe_dataflow_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
        hooks_total_before: usize,
        hooks_total_after: usize,
        access_hooks_before: usize,
        access_hooks_after: usize,
    ) {
        if !self.unsafe_dataflow_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let access_dropped = access_hooks_before.saturating_sub(access_hooks_after);
        let total_dropped = hooks_total_before.saturating_sub(hooks_total_after);
        eprintln!(
            "[rusteze][unsafe-dflow][fn] crate={} fn={} enabled={} tainted_ptrs={} total_ptrs={} access_hooks {}->{} dropped={} total_hooks {}->{} dropped={}",
            crate_name,
            fn_name,
            unsafe_influence.enabled(),
            unsafe_influence.tainted_ptr_count(),
            unsafe_influence.total_ptr_count(),
            access_hooks_before,
            access_hooks_after,
            access_dropped,
            hooks_total_before,
            hooks_total_after,
            total_dropped
        );

        static STATS: OnceLock<Mutex<UnsafeDflowStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeDflowStats::default()))
            .lock()
            .unwrap();

        stats.functions_seen += 1;
        if unsafe_influence.enabled() {
            stats.functions_enabled += 1;
        }
        stats.ptr_locals_tainted_total += unsafe_influence.tainted_ptr_count();
        stats.ptr_locals_total += unsafe_influence.total_ptr_count();
        stats.hooks_total_before += hooks_total_before;
        stats.hooks_total_after += hooks_total_after;
        stats.access_hooks_before += access_hooks_before;
        stats.access_hooks_after += access_hooks_after;

        eprintln!(
            "[rusteze][unsafe-dflow][totals] crate={} fns={} enabled_fns={} ptr_locals tainted/total={}/{} access_hooks {}->{} dropped={} total_hooks {}->{} dropped={}",
            crate_name,
            stats.functions_seen,
            stats.functions_enabled,
            stats.ptr_locals_tainted_total,
            stats.ptr_locals_total,
            stats.access_hooks_before,
            stats.access_hooks_after,
            stats
                .access_hooks_before
                .saturating_sub(stats.access_hooks_after),
            stats.hooks_total_before,
            stats.hooks_total_after,
            stats
                .hooks_total_before
                .saturating_sub(stats.hooks_total_after)
        );
    }

    pub(in crate::instrumentation) fn log_unsafe_dataflow_call_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_call_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let call_stats = unsafe_influence.call_stats();

        eprintln!(
            "[rusteze][unsafe-call][fn] crate={} fn={} seed_arg_unknown={} seed_arg_local_missing={} seed_arg_direct_sink={} seed_arg_escape_unknown_direct={} seed_arg_escape_unknown_inherited={} seed_arg_raw_fallback={} backward_dst_unknown={} backward_dst_local_missing={} backward_dst_forward_to_return={}",
            crate_name,
            fn_name,
            call_stats.seed_arg_unknown_boundary,
            call_stats.seed_arg_local_summary_missing,
            call_stats.seed_arg_summary_direct_sink,
            call_stats.seed_arg_summary_escape_unknown_direct,
            call_stats.seed_arg_summary_escape_unknown_inherited,
            call_stats.seed_arg_raw_fallback,
            call_stats.backward_dst_unknown_boundary,
            call_stats.backward_dst_local_summary_missing,
            call_stats.backward_dst_forward_to_return,
        );

        static STATS: OnceLock<Mutex<UnsafeCallDflowStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeCallDflowStats::default()))
            .lock()
            .unwrap();

        stats.seed_arg_unknown_boundary += call_stats.seed_arg_unknown_boundary;
        stats.seed_arg_local_summary_missing += call_stats.seed_arg_local_summary_missing;
        stats.seed_arg_summary_direct_sink += call_stats.seed_arg_summary_direct_sink;
        stats.seed_arg_summary_escape_unknown_direct +=
            call_stats.seed_arg_summary_escape_unknown_direct;
        stats.seed_arg_summary_escape_unknown_inherited +=
            call_stats.seed_arg_summary_escape_unknown_inherited;
        stats.seed_arg_raw_fallback += call_stats.seed_arg_raw_fallback;
        stats.backward_dst_unknown_boundary += call_stats.backward_dst_unknown_boundary;
        stats.backward_dst_local_summary_missing += call_stats.backward_dst_local_summary_missing;
        stats.backward_dst_forward_to_return += call_stats.backward_dst_forward_to_return;
        for (callee, counts) in &call_stats.unknown_callees {
            let entry = stats.unknown_callees.entry(callee.clone()).or_default();
            entry.0 += counts.seed_arg_unknown_boundary;
            entry.1 += counts.backward_dst_unknown_boundary;
        }

        eprintln!(
            "[rusteze][unsafe-call][totals] crate={} seed_arg_unknown={} seed_arg_local_missing={} seed_arg_direct_sink={} seed_arg_escape_unknown_direct={} seed_arg_escape_unknown_inherited={} seed_arg_raw_fallback={} backward_dst_unknown={} backward_dst_local_missing={} backward_dst_forward_to_return={}",
            crate_name,
            stats.seed_arg_unknown_boundary,
            stats.seed_arg_local_summary_missing,
            stats.seed_arg_summary_direct_sink,
            stats.seed_arg_summary_escape_unknown_direct,
            stats.seed_arg_summary_escape_unknown_inherited,
            stats.seed_arg_raw_fallback,
            stats.backward_dst_unknown_boundary,
            stats.backward_dst_local_summary_missing,
            stats.backward_dst_forward_to_return,
        );
        if self.unsafe_dataflow_unknown_callee_stats_enabled() {
            for (callee, (seed_unknown, backward_unknown)) in stats
                .unknown_callees
                .iter()
                .filter(|(_, counts)| counts.0 != 0 || counts.1 != 0)
            {
                eprintln!(
                    "[rusteze][unsafe-call][unknown] crate={} callee={} seed_arg_unknown={} backward_dst_unknown={}",
                    crate_name, callee, seed_unknown, backward_unknown,
                );
            }
        }
    }

    pub(in crate::instrumentation) fn log_unsafe_dataflow_summary_stats<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_summary_stats_enabled() {
            return;
        }

        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let fn_name = tcx.def_path_str(body.source.def_id());
        let summary = unsafe_influence.summary();
        let ptr_args_total = summary.ptr_args().len();
        let ptr_args_with_direct_sink = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.reaches_direct_sink())
            .count();
        let ptr_args_escaping_unknown = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_unknown_boundary())
            .count();
        let ptr_args_escaping_unknown_direct = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_direct_unknown_boundary())
            .count();
        let ptr_args_escaping_unknown_inherited = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.escapes_to_inherited_unknown_boundary())
            .count();
        let ptr_args_forwarded_to_return = summary
            .ptr_args()
            .iter()
            .filter(|arg| arg.forwarded_to_return())
            .count();

        eprintln!(
            "[rusteze][unsafe-summary][fn] crate={} fn={} direct_sink={} calls_unknown_boundary={} direct_unknown={} inherited_unknown={} ptr_args={} direct_sink_args={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
            crate_name,
            fn_name,
            summary.has_direct_sink(),
            summary.calls_unknown_boundary(),
            summary.calls_unknown_boundary_direct(),
            summary.calls_unknown_boundary_inherited(),
            ptr_args_total,
            ptr_args_with_direct_sink,
            ptr_args_escaping_unknown,
            ptr_args_escaping_unknown_direct,
            ptr_args_escaping_unknown_inherited,
            ptr_args_forwarded_to_return
        );

        for arg in summary.ptr_args() {
            eprintln!(
                "[rusteze][unsafe-summary][arg] crate={} fn={} arg_index={} direct_sink_mask=0x{:x} propagation_mask=0x{:x} direct_sink={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
                crate_name,
                fn_name,
                arg.arg_index,
                arg.direct_sink_mask,
                arg.propagation_mask,
                arg.reaches_direct_sink(),
                arg.escapes_to_unknown_boundary(),
                arg.escapes_to_direct_unknown_boundary(),
                arg.escapes_to_inherited_unknown_boundary(),
                arg.forwarded_to_return()
            );
        }

        static STATS: OnceLock<Mutex<UnsafeSummaryStats>> = OnceLock::new();
        let mut stats = STATS
            .get_or_init(|| Mutex::new(UnsafeSummaryStats::default()))
            .lock()
            .unwrap();

        stats.functions_seen += 1;
        stats.functions_with_direct_sink += usize::from(summary.has_direct_sink());
        stats.functions_calling_unknown_boundary += usize::from(summary.calls_unknown_boundary());
        stats.functions_calling_unknown_boundary_direct +=
            usize::from(summary.calls_unknown_boundary_direct());
        stats.functions_calling_unknown_boundary_inherited +=
            usize::from(summary.calls_unknown_boundary_inherited());
        stats.ptr_args_total += ptr_args_total;
        stats.ptr_args_with_direct_sink += ptr_args_with_direct_sink;
        stats.ptr_args_escaping_unknown += ptr_args_escaping_unknown;
        stats.ptr_args_escaping_unknown_direct += ptr_args_escaping_unknown_direct;
        stats.ptr_args_escaping_unknown_inherited += ptr_args_escaping_unknown_inherited;
        stats.ptr_args_forwarded_to_return += ptr_args_forwarded_to_return;

        eprintln!(
            "[rusteze][unsafe-summary][totals] crate={} fns={} direct_sink_fns={} calls_unknown_boundary_fns={} direct_unknown_fns={} inherited_unknown_fns={} ptr_args={} direct_sink_args={} escape_unknown={} direct_escape_unknown={} inherited_escape_unknown={} to_return={}",
            crate_name,
            stats.functions_seen,
            stats.functions_with_direct_sink,
            stats.functions_calling_unknown_boundary,
            stats.functions_calling_unknown_boundary_direct,
            stats.functions_calling_unknown_boundary_inherited,
            stats.ptr_args_total,
            stats.ptr_args_with_direct_sink,
            stats.ptr_args_escaping_unknown,
            stats.ptr_args_escaping_unknown_direct,
            stats.ptr_args_escaping_unknown_inherited,
            stats.ptr_args_forwarded_to_return
        );
    }

    pub(in crate::instrumentation) fn unsafe_summary_dump_path<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
    ) -> PathBuf {
        let crate_name = tcx.crate_name(LOCAL_CRATE).as_str().replace('-', "_");
        if let Ok(path) = std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP_PATH") {
            return PathBuf::from(path);
        }
        let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
        PathBuf::from(target_dir)
            .join("rusteze-unsafe-summaries")
            .join(format!("{crate_name}.jsonl"))
    }

    pub(in crate::instrumentation) fn dump_unsafe_dataflow_summary<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        unsafe_influence: &UnsafeInfluence,
    ) {
        if !self.unsafe_dataflow_summary_dump_enabled() {
            return;
        }

        let path = self.unsafe_summary_dump_path(tcx);
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                rz_pass_warn!(
                    self,
                    "[rusteze][unsafe-summary] failed to create dump dir {}: {}",
                    parent.display(),
                    err
                );
                return;
            }
        }

        let fn_name = tcx.def_path_str(body.source.def_id());
        let fn_hash = {
            let hash = tcx.def_path_hash(body.source.def_id());
            format!("{:x}:{:x}", hash.stable_crate_id(), hash.local_hash())
        };
        let (trait_fn_name, trait_fn_hash) = tcx
            .opt_associated_item(body.source.def_id())
            .and_then(|item| item.trait_item_def_id)
            .filter(|trait_did| *trait_did != body.source.def_id())
            .map(|trait_did| {
                let hash = tcx.def_path_hash(trait_did);
                (
                    tcx.def_path_str(trait_did),
                    format!("{:x}:{:x}", hash.stable_crate_id(), hash.local_hash()),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new()));
        let crate_name_sym = tcx.crate_name(LOCAL_CRATE);
        let crate_name = crate_name_sym.as_str();
        let summary = unsafe_influence.summary();
        let record = UnsafeSummaryRecord {
            crate_name: crate_name.to_string(),
            function: fn_name,
            function_hash: fn_hash,
            trait_function: trait_fn_name,
            trait_function_hash: trait_fn_hash,
            has_direct_sink: summary.has_direct_sink(),
            calls_unknown_boundary: summary.calls_unknown_boundary(),
            calls_unknown_boundary_direct: summary.calls_unknown_boundary_direct(),
            calls_unknown_boundary_inherited: summary.calls_unknown_boundary_inherited(),
            ptr_args: summary.ptr_args().to_vec(),
            local_callsites: unsafe_influence.local_callsites().to_vec(),
        };
        let Ok(mut line) = serde_json::to_string(&record) else {
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-summary] failed to serialize summary"
            );
            return;
        };
        line.push('\n');

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                if let Err(err) = file.write_all(line.as_bytes()) {
                    rz_pass_warn!(
                        self,
                        "[rusteze][unsafe-summary] failed to write {}: {}",
                        path.display(),
                        err
                    );
                }
            }
            Err(err) => {
                rz_pass_warn!(
                    self,
                    "[rusteze][unsafe-summary] failed to open {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    pub(in crate::instrumentation) fn filter_insert_points_by_unsafe_dataflow<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        mut insert_points: Vec<InsertPoint<'tcx>>,
        unsafe_influence: &UnsafeInfluence,
    ) -> Vec<InsertPoint<'tcx>> {
        let before_total = insert_points.len();
        let before_access = insert_points
            .iter()
            .filter(|ip| {
                matches!(
                    ip.kind,
                    InstrKind::PtrRead { .. }
                        | InstrKind::PtrWrite { .. }
                        | InstrKind::PtrReadAllowUntagged { .. }
                        | InstrKind::PtrWriteAllowUntagged { .. }
                        | InstrKind::PtrUse { .. }
                )
            })
            .count();

        if !unsafe_influence.enabled() {
            self.log_unsafe_dataflow_stats(
                tcx,
                body,
                unsafe_influence,
                before_total,
                before_total,
                before_access,
                before_access,
            );
            self.log_unsafe_dataflow_call_stats(tcx, body, unsafe_influence);
            return insert_points;
        }

        insert_points.retain(|ip| {
            if !matches!(
                ip.kind,
                InstrKind::PtrRead { .. }
                    | InstrKind::PtrWrite { .. }
                    | InstrKind::PtrReadAllowUntagged { .. }
                    | InstrKind::PtrWriteAllowUntagged { .. }
            ) {
                return true;
            }
            let ptr_local_opt = Self::unsafe_dataflow_gated_local(&ip.kind, &ip.place);
            ptr_local_opt
                .filter(|&l| self.is_raw_pointer_ty(body.local_decls[l].ty))
                .map(|l| unsafe_influence.should_instrument_ptr_local(l))
                .unwrap_or(true)
        });

        let after_total = insert_points.len();
        let after_access = insert_points
            .iter()
            .filter(|ip| {
                matches!(
                    ip.kind,
                    InstrKind::PtrRead { .. }
                        | InstrKind::PtrWrite { .. }
                        | InstrKind::PtrReadAllowUntagged { .. }
                        | InstrKind::PtrWriteAllowUntagged { .. }
                        | InstrKind::PtrUse { .. }
                )
            })
            .count();

        if self.trace_unsafe_dataflow_enabled() {
            let dropped = before_total.saturating_sub(after_total);
            rz_pass_warn!(
                self,
                "[rusteze][unsafe-dflow] filtered {} access hooks (kept {} / tainted_ptrs={} total_ptrs={})",
                dropped,
                after_total,
                unsafe_influence.tainted_ptr_count(),
                unsafe_influence.total_ptr_count()
            );
        }

        self.log_unsafe_dataflow_stats(
            tcx,
            body,
            unsafe_influence,
            before_total,
            after_total,
            before_access,
            after_access,
        );
        self.log_unsafe_dataflow_call_stats(tcx, body, unsafe_influence);

        insert_points
    }
}
