//! Interprocedural unsafe-dataflow analysis used by Rusteze pruning.
//!
//! This is the summary-based analysis behind the "3-phase interprocedural
//! analysis" mode:
//! 1. analyze functions and dump local summaries
//! 2. merge summaries across crates / call edges
//! 3. rebuild while consuming merged summaries
//!
//! User-facing activation:
//! - `RZ_INTERPROC_UNSAFE_SUMMARIES=1`
//!
//! This pass is distinct from `instrumentation/metadata_dataflow.rs`:
//! - `unsafe_dataflow.rs` answers "which pointer locals are semantically
//!   relevant enough that we must keep certain dynamic checks?"
//! - `metadata_dataflow.rs` answers "which metadata-copy hooks are redundant
//!   once we have already decided to instrument this function?"
//!
//! The current supported pruning boundary is intentionally narrow and sound by
//! default:
//! - prune only raw-pointer access hooks proved irrelevant by the analysis
//! - do not prune provenance/lineage-creating hooks by default
//!
//! This file therefore models pointer relevance, call effects, summaries, and
//! unknown-boundary conservatism. Missing precision is acceptable; unsound
//! pruning is not.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_middle::mir::{
    BasicBlockData, Body, CastKind, Local, Operand, Place, ProjectionElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, RETURN_PLACE,
};
use rustc_middle::ty::{GenericArgsRef, Instance, TyCtxt, TyKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub(crate) struct UnsafeInfluence {
    enabled: bool,
    tainted_ptr_locals: HashSet<Local>,
    total_ptr_locals: usize,
    summary: UnsafeFunctionSummary,
    local_callsites: Vec<UnsafeCallsiteSummary>,
    call_stats: UnsafeCallRelevanceStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsafeFunctionSummary {
    has_direct_sink: bool,
    calls_unknown_boundary_direct: bool,
    calls_unknown_boundary_inherited: bool,
    ptr_args: Vec<UnsafeArgSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsafeArgSummary {
    pub(crate) arg_index: usize,
    pub(crate) direct_sink_mask: u32,
    pub(crate) propagation_mask: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsafeArgEdge {
    pub(crate) caller_arg_index: usize,
    pub(crate) callee_arg_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsafeCallsiteSummary {
    pub(crate) callee_crate_name: String,
    pub(crate) callee_function: String,
    #[serde(default)]
    pub(crate) callee_function_hash: String,
    pub(crate) arg_edges: Vec<UnsafeArgEdge>,
    pub(crate) return_to_return: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UnsafeSummaryRecord {
    pub(crate) crate_name: String,
    pub(crate) function: String,
    #[serde(default)]
    pub(crate) function_hash: String,
    #[serde(default)]
    pub(crate) trait_function: String,
    #[serde(default)]
    pub(crate) trait_function_hash: String,
    pub(crate) has_direct_sink: bool,
    pub(crate) calls_unknown_boundary: bool,
    pub(crate) calls_unknown_boundary_direct: bool,
    pub(crate) calls_unknown_boundary_inherited: bool,
    pub(crate) ptr_args: Vec<UnsafeArgSummary>,
    pub(crate) local_callsites: Vec<UnsafeCallsiteSummary>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UnsafeCallRelevanceStats {
    pub(crate) seed_arg_unknown_boundary: usize,
    pub(crate) seed_arg_local_summary_missing: usize,
    pub(crate) seed_arg_summary_direct_sink: usize,
    pub(crate) seed_arg_summary_escape_unknown_direct: usize,
    pub(crate) seed_arg_summary_escape_unknown_inherited: usize,
    pub(crate) seed_arg_raw_fallback: usize,
    pub(crate) backward_dst_unknown_boundary: usize,
    pub(crate) backward_dst_local_summary_missing: usize,
    pub(crate) backward_dst_forward_to_return: usize,
    pub(crate) unknown_callees: BTreeMap<String, UnsafeUnknownCalleeStats>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UnsafeUnknownCalleeStats {
    pub(crate) seed_arg_unknown_boundary: usize,
    pub(crate) backward_dst_unknown_boundary: usize,
}

impl UnsafeInfluence {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            tainted_ptr_locals: HashSet::new(),
            total_ptr_locals: 0,
            summary: UnsafeFunctionSummary::default(),
            local_callsites: Vec::new(),
            call_stats: UnsafeCallRelevanceStats::default(),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn tainted_ptr_count(&self) -> usize {
        self.tainted_ptr_locals.len()
    }

    pub(crate) fn total_ptr_count(&self) -> usize {
        self.total_ptr_locals
    }

    pub(crate) fn should_instrument_ptr_local(&self, local: Local) -> bool {
        if !self.enabled {
            return true;
        }
        self.tainted_ptr_locals.contains(&local)
    }

    pub(crate) fn summary(&self) -> &UnsafeFunctionSummary {
        &self.summary
    }

    pub(crate) fn local_callsites(&self) -> &[UnsafeCallsiteSummary] {
        &self.local_callsites
    }

    pub(crate) fn call_stats(&self) -> &UnsafeCallRelevanceStats {
        &self.call_stats
    }
}

impl UnsafeFunctionSummary {
    pub(crate) fn has_direct_sink(&self) -> bool {
        self.has_direct_sink
    }

    pub(crate) fn calls_unknown_boundary(&self) -> bool {
        self.calls_unknown_boundary_direct || self.calls_unknown_boundary_inherited
    }

    pub(crate) fn calls_unknown_boundary_direct(&self) -> bool {
        self.calls_unknown_boundary_direct
    }

    pub(crate) fn calls_unknown_boundary_inherited(&self) -> bool {
        self.calls_unknown_boundary_inherited
    }

    pub(crate) fn ptr_args(&self) -> &[UnsafeArgSummary] {
        &self.ptr_args
    }
}

impl UnsafeArgSummary {
    pub(crate) const DIRECT_RAW_DEREF: u32 = 1 << 0;
    pub(crate) const DIRECT_RAW_CREATION: u32 = 1 << 1;
    pub(crate) const DIRECT_PROVENANCE_CAST: u32 = 1 << 2;
    pub(crate) const DIRECT_RAW_ARG_TO_CALL: u32 = 1 << 3;

    pub(crate) const PROP_ESCAPE_UNKNOWN: u32 = 1 << 16;
    pub(crate) const PROP_FORWARD_TO_RETURN: u32 = 1 << 17;
    pub(crate) const PROP_ESCAPE_UNKNOWN_INHERITED: u32 = 1 << 18;

    pub(crate) fn reaches_direct_sink(&self) -> bool {
        self.direct_sink_mask != 0
    }

    pub(crate) fn escapes_to_unknown_boundary(&self) -> bool {
        self.escapes_to_direct_unknown_boundary() || self.escapes_to_inherited_unknown_boundary()
    }

    pub(crate) fn escapes_to_direct_unknown_boundary(&self) -> bool {
        (self.propagation_mask & Self::PROP_ESCAPE_UNKNOWN) != 0
    }

    pub(crate) fn escapes_to_inherited_unknown_boundary(&self) -> bool {
        (self.propagation_mask & Self::PROP_ESCAPE_UNKNOWN_INHERITED) != 0
    }

    pub(crate) fn forwarded_to_return(&self) -> bool {
        (self.propagation_mask & Self::PROP_FORWARD_TO_RETURN) != 0
    }

    pub(crate) fn arg_index(&self) -> usize {
        self.arg_index
    }

    pub(crate) fn direct_sink_mask(&self) -> u32 {
        self.direct_sink_mask
    }

    pub(crate) fn propagation_mask(&self) -> u32 {
        self.propagation_mask
    }
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn warn_legacy_local_unsafe_dataflow_if_requested() {
    static WARN_ONCE: OnceLock<()> = OnceLock::new();
    if env_flag_enabled("RZ_UNSAFE_DATAFLOW") {
        let _ = WARN_ONCE.get_or_init(|| {
            eprintln!(
                "[rusteze] warning: RZ_UNSAFE_DATAFLOW is no longer a supported activation path and is ignored; use RZ_INTERPROC_UNSAFE_SUMMARIES=1 or the analyze/use summary flags instead"
            );
        });
    }
}

pub(crate) fn analyze_unsafe_summaries_enabled() -> bool {
    env_flag_enabled("RZ_ANALYZE_UNSAFE_SUMMARIES")
}

pub(crate) fn unsafe_dataflow_enabled() -> bool {
    warn_legacy_local_unsafe_dataflow_if_requested();

    // Supported activation paths:
    //  - analyze-only summary generation
    //  - normal builds consuming merged summaries
    //
    // Plain `RZ_UNSAFE_DATAFLOW=1` without summaries is intentionally ignored
    // because the local-only path is not sound enough for the full example suite.
    analyze_unsafe_summaries_enabled() || use_loaded_unsafe_summaries_enabled()
}

pub(crate) fn use_loaded_unsafe_summaries_enabled() -> bool {
    env_flag_enabled("RZ_USE_UNSAFE_SUMMARIES")
}

fn trace_local_summary_missing_enabled() -> bool {
    env_flag_enabled("RZ_TRACE_LOCAL_SUMMARY_MISSING")
}

fn unknown_callee_stats_enabled() -> bool {
    env_flag_enabled("RZ_UNSAFE_DATAFLOW_UNKNOWN_CALLEE_STATS")
}

fn summary_input_dir() -> PathBuf {
    if let Ok(path) = std::env::var("RZ_UNSAFE_SUMMARY_INPUT_DIR") {
        return PathBuf::from(path);
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target_dir).join("rusteze-unsafe-summaries")
}

fn def_path_hash_key<'tcx>(tcx: TyCtxt<'tcx>, did: DefId) -> String {
    let hash = tcx.def_path_hash(did);
    format!("{:x}:{:x}", hash.stable_crate_id(), hash.local_hash())
}

fn loaded_unsafe_summaries() -> &'static HashMap<(String, String), UnsafeFunctionSummary> {
    static LOADED: OnceLock<HashMap<(String, String), UnsafeFunctionSummary>> = OnceLock::new();
    LOADED.get_or_init(|| {
        let mut map = HashMap::new();
        let dir = summary_input_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return map;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(record) = serde_json::from_str::<UnsafeSummaryRecord>(line) else {
                    continue;
                };
                let summary = UnsafeFunctionSummary {
                    has_direct_sink: record.has_direct_sink,
                    calls_unknown_boundary_direct: record.calls_unknown_boundary_direct
                        || (record.calls_unknown_boundary
                            && !record.calls_unknown_boundary_inherited),
                    calls_unknown_boundary_inherited: record.calls_unknown_boundary_inherited,
                    ptr_args: record.ptr_args,
                };
                let function_key = if record.function_hash.is_empty() {
                    record.function.clone()
                } else {
                    record.function_hash.clone()
                };
                let crate_name = record.crate_name;
                map.insert((crate_name.clone(), function_key), summary.clone());
                if !record.trait_function_hash.is_empty() {
                    merge_summary(
                        map.entry((crate_name, record.trait_function_hash))
                            .or_default(),
                        &summary,
                    );
                }
            }
        }
        map
    })
}

fn merge_summary(dst: &mut UnsafeFunctionSummary, src: &UnsafeFunctionSummary) {
    dst.has_direct_sink |= src.has_direct_sink;
    dst.calls_unknown_boundary_direct |= src.calls_unknown_boundary_direct;
    dst.calls_unknown_boundary_inherited |= src.calls_unknown_boundary_inherited;
    for src_arg in &src.ptr_args {
        match dst
            .ptr_args
            .iter_mut()
            .find(|arg| arg.arg_index == src_arg.arg_index)
        {
            Some(dst_arg) => {
                dst_arg.direct_sink_mask |= src_arg.direct_sink_mask;
                dst_arg.propagation_mask |= src_arg.propagation_mask;
            }
            None => dst.ptr_args.push(src_arg.clone()),
        }
    }
    dst.ptr_args.sort_by_key(|arg| arg.arg_index);
}

fn is_pointer_ty<'tcx>(ty: rustc_middle::ty::Ty<'tcx>) -> bool {
    matches!(ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..))
}

fn is_raw_pointer_ty<'tcx>(ty: rustc_middle::ty::Ty<'tcx>) -> bool {
    matches!(ty.kind(), TyKind::RawPtr(..))
}

fn place_from_operand<'tcx>(op: &Operand<'tcx>) -> Option<Place<'tcx>> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => Some(*p),
        _ => None,
    }
}

fn place_starts_with_deref<'tcx>(p: &Place<'tcx>) -> bool {
    p.projection
        .iter()
        .next()
        .is_some_and(|pe| matches!(pe, ProjectionElem::Deref))
}

fn simple_local_copy_source<'tcx>(rhs: &Rvalue<'tcx>) -> Option<Local> {
    match rhs {
        Rvalue::Use(op) => place_from_operand(op).and_then(|p| p.as_local()),
        _ => None,
    }
}

fn taint_local(local: Local, tainted_ptr_locals: &mut HashSet<Local>) -> bool {
    tainted_ptr_locals.insert(local)
}

fn taint_value_local(local: Local, tainted_value_locals: &mut HashSet<Local>) -> bool {
    tainted_value_locals.insert(local)
}

fn mark_ptr_local(local: Local, marked_ptr_locals: &mut HashSet<Local>) -> bool {
    marked_ptr_locals.insert(local)
}

fn mark_value_local(local: Local, marked_value_locals: &mut HashSet<Local>) -> bool {
    marked_value_locals.insert(local)
}

fn operand_tainted<'tcx>(op: &Operand<'tcx>, tainted_value_locals: &HashSet<Local>) -> bool {
    place_from_operand(op)
        .map(|p| tainted_value_locals.contains(&p.local))
        .unwrap_or(false)
}

fn rvalue_tainted<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    rv: &Rvalue<'tcx>,
    tainted_value_locals: &HashSet<Local>,
) -> bool {
    match rv {
        Rvalue::Use(op) | Rvalue::Repeat(op, _) => operand_tainted(op, tainted_value_locals),
        Rvalue::RawPtr(_, p) => {
            // Raw pointer construction is an unsafe-influence root.
            tainted_value_locals.contains(&p.local)
                || is_pointer_ty(p.ty(&body.local_decls, tcx).ty)
        }
        Rvalue::Ref(_, _, p) | Rvalue::CopyForDeref(p) => tainted_value_locals.contains(&p.local),
        Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => {
            operand_tainted(op, tainted_value_locals)
        }
        Rvalue::BinaryOp(_, ops) => {
            operand_tainted(&ops.0, tainted_value_locals)
                || operand_tainted(&ops.1, tainted_value_locals)
        }
        Rvalue::Aggregate(_, ops) => ops
            .iter()
            .any(|op| operand_tainted(op, tainted_value_locals)),
        _ => false,
    }
}

fn rvalue_is_unsafe_root<'tcx>(body: &Body<'tcx>, rv: &Rvalue<'tcx>) -> bool {
    match rv {
        Rvalue::RawPtr(..) => true,
        Rvalue::Cast(CastKind::PointerWithExposedProvenance, _, to_ty) => is_pointer_ty(*to_ty),
        Rvalue::Cast(CastKind::Transmute, op, to_ty) => {
            if !is_pointer_ty(*to_ty) {
                return false;
            }
            place_from_operand(op)
                .map(|p| body.local_decls[p.local].ty.is_integral())
                .unwrap_or(true)
        }
        _ => false,
    }
}

fn parse_instrumented_crates_env() -> Option<HashSet<String>> {
    let raw = std::env::var("RZ_INSTRUMENTED_CRATES").ok()?;
    let mut set = HashSet::new();
    for part in raw.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        // Allow either '-' or '_' in names; rustc uses '_' for crate_name().
        set.insert(p.replace('-', "_"));
    }
    Some(set)
}

fn instrument_all_deps_enabled() -> bool {
    std::env::var("RZ_INSTRUMENT_ALL_DEPS")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn is_std_like_crate_name(name: &str) -> bool {
    matches!(name, "core" | "std")
}

fn instrumented_crates_cached<'tcx>(tcx: TyCtxt<'tcx>) -> &'static HashSet<String> {
    static INSTRUMENTED: OnceLock<HashSet<String>> = OnceLock::new();

    INSTRUMENTED.get_or_init(|| {
        // Priority 1: explicit allowlist
        if let Some(env_set) = parse_instrumented_crates_env() {
            return env_set;
        }

        // Priority 2: instrument all non-runtime dependencies
        if !instrument_all_deps_enabled() {
            return HashSet::new();
        }

        let mut set = HashSet::new();
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum).as_str().to_string();
            if name == "runtime" {
                continue;
            }

            // Even in "instrument all deps" mode, do NOT treat std/core as instrumented
            // callees. We rely on wrapper classification there.
            if is_std_like_crate_name(&name) {
                continue;
            }

            set.insert(name);
        }
        set
    })
}

fn resolve_callee_def_id<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> Option<rustc_hir::def_id::DefId> {
    fn resolve_instance_def_id<'tcx>(
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        def_id: DefId,
        args: GenericArgsRef<'tcx>,
    ) -> DefId {
        let typing_env = body.typing_env(tcx);
        let normalized_args = tcx
            .try_normalize_erasing_regions(typing_env, args)
            .unwrap_or(args);
        Instance::try_resolve(tcx, typing_env, def_id, normalized_args)
            .ok()
            .flatten()
            .map(|instance| instance.def_id())
            .unwrap_or(def_id)
    }

    match func {
        Operand::Constant(c) => match c.const_.ty().kind() {
            TyKind::FnDef(def_id, args) => Some(resolve_instance_def_id(tcx, body, *def_id, args)),
            _ => None,
        },
        Operand::Copy(p) | Operand::Move(p) => match p.ty(&body.local_decls, tcx).ty.kind() {
            TyKind::FnDef(def_id, args) => Some(resolve_instance_def_id(tcx, body, *def_id, args)),
            _ => None,
        },
        _ => None,
    }
}

fn make_pointer_arg_summary(
    arg_index: usize,
    direct_sink_mask: u32,
    propagation_mask: u32,
) -> UnsafeArgSummary {
    UnsafeArgSummary {
        arg_index,
        direct_sink_mask,
        propagation_mask,
    }
}

fn known_external_summary<'tcx>(
    tcx: TyCtxt<'tcx>,
    did: rustc_hir::def_id::DefId,
) -> Option<UnsafeFunctionSummary> {
    let crate_name_sym = tcx.crate_name(did.krate);
    let crate_name = crate_name_sym.as_str();
    if !matches!(crate_name, "core" | "std" | "alloc") {
        return None;
    }

    let path = tcx.def_path_str(did);
    let sig = tcx.fn_sig(did).instantiate_identity().skip_binder();
    let ptr_arg_indices: Vec<usize> = sig
        .inputs()
        .iter()
        .enumerate()
        .filter_map(|(idx, ty)| is_pointer_ty(*ty).then_some(idx))
        .collect();

    let arg0_ptr = ptr_arg_indices.first().copied();
    let mut summary = UnsafeFunctionSummary::default();

    // Pure forwarding helpers on refs/slices/containers.
    if matches!(
        path.as_str(),
        p if p.ends_with("::as_ref")
            || p.ends_with("::as_mut")
            || p.ends_with("::as_slice")
            || p.ends_with("::as_mut_slice")
            || p.ends_with("::borrow")
            || p.ends_with("::into")
            || p.ends_with("::deref")
            || p.ends_with("::deref_mut")
            || p.ends_with("::index")
            || p.ends_with("::index_mut")
            || p.ends_with("::fill_buf")
            || p.ends_with("::from_utf8")
            || p.ends_with("::get")
            || p.ends_with("::strip_suffix")
            || p.ends_with("::strip_prefix")
    ) {
        if let Some(arg_index) = arg0_ptr {
            summary.ptr_args.push(make_pointer_arg_summary(
                arg_index,
                0,
                UnsafeArgSummary::PROP_FORWARD_TO_RETURN,
            ));
        }
        return Some(summary);
    }

    // Simple state-mutating helpers that should not taint caller pointer provenance.
    if matches!(
        path.as_str(),
        p if p.ends_with("::write_str")
            || p.ends_with("::write_fmt")
            || p.ends_with("::consume")
            || p.ends_with("::kind")
            || p.ends_with("::size_of_val")
            || p.ends_with("::bswap")
            || p.ends_with("::starts_with")
            || p.ends_with("::ends_with")
            || p.ends_with("::eq_ignore_ascii_case")
            || p.ends_with("::finish")
            || p.ends_with("::fmt")
    ) {
        return Some(summary);
    }

    // Builder-style formatting helpers return the builder reference in arg0 and otherwise should
    // not taint caller pointer provenance.
    if matches!(
        path.as_str(),
        p if p.ends_with("::field")
            || p.ends_with("::debug_struct")
            || p.ends_with("::debug_tuple")
    ) {
        if let Some(arg_index) = arg0_ptr {
            summary.ptr_args.push(make_pointer_arg_summary(
                arg_index,
                0,
                UnsafeArgSummary::PROP_FORWARD_TO_RETURN,
            ));
        }
        return Some(summary);
    }

    // Pointer/view creators that return a raw pointer derived from arg0.
    if matches!(
        path.as_str(),
        p if p.ends_with("::as_ptr")
            || p.ends_with("::as_mut_ptr")
            || p.ends_with("::as_non_null")
            || p.ends_with("::as_ptr_range")
            || p.ends_with("::as_mut_ptr_range")
            || p.ends_with("::add")
            || p.ends_with("::sub")
            || p.ends_with("::byte_add")
            || p.ends_with("::byte_sub")
            || p.ends_with("::wrapping_add")
            || p.ends_with("::wrapping_sub")
            || p.ends_with("::offset")
            || p.ends_with("::wrapping_offset")
    ) {
        if let Some(arg_index) = arg0_ptr {
            summary.has_direct_sink = true;
            summary.ptr_args.push(make_pointer_arg_summary(
                arg_index,
                UnsafeArgSummary::DIRECT_RAW_CREATION,
                UnsafeArgSummary::PROP_FORWARD_TO_RETURN,
            ));
        }
        return Some(summary);
    }

    // Raw/value comparisons read through their pointer-like inputs but do not otherwise
    // introduce unknown call effects. Model them as direct sinks on the pointer args.
    if path.contains("compare_bytes")
        || path.ends_with("::equal")
        || path.ends_with("::eq")
        || path.ends_with("::cmp")
        || path.ends_with("::partial_cmp")
        || path.ends_with("::hash")
        || path.ends_with("::atomic_load")
    {
        summary.has_direct_sink = true;
        for arg_index in ptr_arg_indices {
            summary.ptr_args.push(make_pointer_arg_summary(
                arg_index,
                UnsafeArgSummary::DIRECT_RAW_ARG_TO_CALL,
                0,
            ));
        }
        return Some(summary);
    }

    // core::ptr / std::ptr direct raw sinks.
    if (path.contains("core::ptr::") || path.contains("std::ptr::"))
        && matches!(
            path.as_str(),
            p if p.ends_with("::read")
                || p.ends_with("::read_unaligned")
                || p.ends_with("::read_volatile")
                || p.ends_with("::write")
                || p.ends_with("::write_unaligned")
                || p.ends_with("::write_volatile")
                || p.ends_with("::copy")
                || p.ends_with("::copy_nonoverlapping")
                || p.ends_with("::swap")
                || p.ends_with("::replace")
        )
    {
        summary.has_direct_sink = true;
        for arg_index in ptr_arg_indices {
            summary.ptr_args.push(make_pointer_arg_summary(
                arg_index,
                UnsafeArgSummary::DIRECT_RAW_ARG_TO_CALL,
                0,
            ));
        }
        return Some(summary);
    }

    None
}

pub(crate) fn callee_summary<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> Option<UnsafeFunctionSummary> {
    let did = resolve_callee_def_id(tcx, body, func)?;
    if use_loaded_unsafe_summaries_enabled() {
        let crate_name = tcx.crate_name(did.krate).as_str().to_string();
        let function_hash = def_path_hash_key(tcx, did);
        if let Some(summary) = loaded_unsafe_summaries().get(&(crate_name, function_hash)) {
            return Some(summary.clone());
        }
    }
    if did.krate != LOCAL_CRATE {
        known_external_summary(tcx, did)
    } else {
        None
    }
}

pub(crate) fn summary_for_def_id<'tcx>(
    tcx: TyCtxt<'tcx>,
    did: DefId,
) -> Option<UnsafeFunctionSummary> {
    if use_loaded_unsafe_summaries_enabled() {
        let crate_name = tcx.crate_name(did.krate).as_str().to_string();
        let function_hash = def_path_hash_key(tcx, did);
        if let Some(summary) = loaded_unsafe_summaries().get(&(crate_name, function_hash)) {
            return Some(summary.clone());
        }
    }

    if did.krate != LOCAL_CRATE {
        known_external_summary(tcx, did)
    } else {
        None
    }
}

fn trace_local_summary_missing<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>, func: &Operand<'tcx>) {
    if !trace_local_summary_missing_enabled() || !use_loaded_unsafe_summaries_enabled() {
        return;
    }
    let Some(did) = resolve_callee_def_id(tcx, body, func) else {
        return;
    };
    if did.krate != LOCAL_CRATE {
        return;
    }
    let caller = tcx.def_path_str(body.source.def_id());
    let crate_name = tcx.crate_name(did.krate).as_str().to_string();
    let function = tcx.def_path_str(did);
    let function_hash = def_path_hash_key(tcx, did);
    let summaries = loaded_unsafe_summaries();
    let exact = summaries.contains_key(&(crate_name.clone(), function_hash.clone()));
    eprintln!(
        "[rusteze][unsafe-call][local-missing] crate={} caller={} callee={} callee_hash={} exact_match={} loaded_summary_count={}",
        crate_name,
        caller,
        function,
        function_hash,
        exact,
        summaries.len(),
    );
}

fn instrumented_call_boundary<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> bool {
    let Some(did) = resolve_callee_def_id(tcx, body, func) else {
        // Callee could not be resolved (fn ptr / vtable / etc.): conservatively unknown.
        return false;
    };

    if did.krate == LOCAL_CRATE {
        return true;
    }

    let crate_name_sym = tcx.crate_name(did.krate);
    let crate_name = crate_name_sym.as_str();
    if crate_name == "runtime" || is_std_like_crate_name(crate_name) {
        return false;
    }

    instrumented_crates_cached(tcx).contains(crate_name)
}

fn unknown_callee_label<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> String {
    match resolve_callee_def_id(tcx, body, func) {
        Some(did) => {
            let crate_name_sym = tcx.crate_name(did.krate);
            let crate_name = crate_name_sym.as_str();
            let path = tcx.def_path_str(did);
            format!("{crate_name}::{path}")
        }
        None => "<unresolved>".to_string(),
    }
}

fn record_unknown_callee_seed<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
    call_stats: &mut UnsafeCallRelevanceStats,
) {
    if !unknown_callee_stats_enabled() {
        return;
    }
    let label = unknown_callee_label(tcx, body, func);
    call_stats
        .unknown_callees
        .entry(label)
        .or_default()
        .seed_arg_unknown_boundary += 1;
}

fn record_unknown_callee_backward<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
    call_stats: &mut UnsafeCallRelevanceStats,
) {
    if !unknown_callee_stats_enabled() {
        return;
    }
    let label = unknown_callee_label(tcx, body, func);
    call_stats
        .unknown_callees
        .entry(label)
        .or_default()
        .backward_dst_unknown_boundary += 1;
}

fn apply_statement<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    stmt: &Statement<'tcx>,
    tainted_value_locals: &mut HashSet<Local>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;

    if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
        // Deref read/write roots through raw pointers.
        if place_starts_with_deref(lhs_place) {
            let ptr_local = lhs_place.local;
            let ptr_ty = body.local_decls[ptr_local].ty;
            if is_raw_pointer_ty(ptr_ty) {
                changed |= taint_value_local(ptr_local, tainted_value_locals);
                changed |= taint_local(ptr_local, tainted_ptr_locals);
            }
        }
        if let Rvalue::Use(op) = rhs {
            if let Some(p) = place_from_operand(op) {
                if place_starts_with_deref(&p) {
                    let ptr_local = p.local;
                    let ptr_ty = body.local_decls[ptr_local].ty;
                    if is_raw_pointer_ty(ptr_ty) {
                        changed |= taint_value_local(ptr_local, tainted_value_locals);
                        changed |= taint_local(ptr_local, tainted_ptr_locals);
                    }
                }
            }
        }

        let rhs_is_tainted = rvalue_tainted(tcx, body, rhs, tainted_value_locals);
        let rhs_is_unsafe_root = rvalue_is_unsafe_root(body, rhs);

        if rhs_is_tainted || rhs_is_unsafe_root {
            changed |= taint_value_local(lhs_place.local, tainted_value_locals);
        }

        if let Some(dst_local) = lhs_place.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if (rhs_is_tainted || rhs_is_unsafe_root) && is_pointer_ty(dst_ty) {
                changed |= taint_local(dst_local, tainted_ptr_locals);
            }
        }
    }

    changed
}

fn apply_terminator<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    term: &Terminator<'tcx>,
    tainted_value_locals: &mut HashSet<Local>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;

    if let TerminatorKind::Call {
        func,
        args,
        destination,
        ..
    } = &term.kind
    {
        let mut ptr_arg_locals: Vec<(usize, Local)> = Vec::new();
        let mut raw_ptr_arg_locals: Vec<Local> = Vec::new();
        let mut any_raw_arg = false;

        for (arg_index, arg) in args.iter().enumerate() {
            if let Some(p) = place_from_operand(&arg.node) {
                let local = p.local;
                let local_ty = body.local_decls[local].ty;
                if is_pointer_ty(local_ty) {
                    ptr_arg_locals.push((arg_index, local));
                    let is_raw = is_raw_pointer_ty(local_ty);
                    if is_raw {
                        raw_ptr_arg_locals.push(local);
                    }
                    any_raw_arg |= is_raw;
                }
            }
        }

        let callee_summary = callee_summary(tcx, body, func);
        // Treat a non-instrumented boundary as "unknown" only when we have no usable summary for
        // it. Hand-modeled std/core helpers and merged interprocedural summaries should sharpen
        // call behavior instead of falling back to blanket taint.
        let unknown_boundary =
            !instrumented_call_boundary(tcx, body, func) && callee_summary.is_none();
        let conservative_local_fallback = !unknown_boundary
            && resolve_callee_def_id(tcx, body, func).is_some_and(|did| did.krate == LOCAL_CRATE)
            && callee_summary.is_none();

        if unknown_boundary {
            for (_, local) in ptr_arg_locals.iter().copied() {
                changed |= taint_value_local(local, tainted_value_locals);
                changed |= taint_local(local, tainted_ptr_locals);
            }
        } else if let Some(summary) = &callee_summary {
            for (arg_index, local) in ptr_arg_locals.iter().copied() {
                if let Some(arg_summary) = summary
                    .ptr_args()
                    .iter()
                    .find(|entry| entry.arg_index() == arg_index)
                {
                    if arg_summary.reaches_direct_sink()
                        || arg_summary.escapes_to_unknown_boundary()
                    {
                        changed |= taint_value_local(local, tainted_value_locals);
                        changed |= taint_local(local, tainted_ptr_locals);
                    }
                }
            }
        } else if any_raw_arg {
            // For known instrumented callees, keep raw-pointer conservativeness but avoid
            // blanket-tainting unrelated shared/reference pointer operands.
            for local in raw_ptr_arg_locals.iter().copied() {
                changed |= taint_value_local(local, tainted_value_locals);
                changed |= taint_local(local, tainted_ptr_locals);
            }
        } else if conservative_local_fallback {
            for (_, local) in ptr_arg_locals.iter().copied() {
                changed |= taint_value_local(local, tainted_value_locals);
                changed |= taint_local(local, tainted_ptr_locals);
            }
        }

        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if is_pointer_ty(dst_ty) {
                let local_returned_from_arg = callee_summary.as_ref().is_some_and(|summary| {
                    ptr_arg_locals.iter().any(|(arg_index, local)| {
                        tainted_value_locals.contains(local)
                            && summary.ptr_args().iter().any(|entry| {
                                entry.arg_index() == *arg_index && entry.forwarded_to_return()
                            })
                    })
                });
                let returned_relevant = if unknown_boundary || conservative_local_fallback {
                    true
                } else if callee_summary.is_some() {
                    local_returned_from_arg
                } else {
                    // For modeled-but-unsummarized instrumented callees, keep only the raw-pointer
                    // fallback conservative. Plain tainted pointer/reference args should not make a
                    // pointer return look relevant unless the callee summary says the return flows
                    // from them.
                    any_raw_arg
                };
                if returned_relevant {
                    changed |= taint_value_local(dst_local, tainted_value_locals);
                    changed |= taint_local(dst_local, tainted_ptr_locals);
                }
            }
        }
    }

    changed
}

fn stmt_direct_sink_mask<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    stmt: &Statement<'tcx>,
    tainted_value_locals: &HashSet<Local>,
) -> u32 {
    if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
        if place_starts_with_deref(lhs_place) {
            let ptr_local = lhs_place.local;
            if tainted_value_locals.contains(&ptr_local)
                && is_raw_pointer_ty(body.local_decls[ptr_local].ty)
            {
                return UnsafeArgSummary::DIRECT_RAW_DEREF;
            }
        }
        if let Rvalue::Use(op) = rhs {
            if let Some(p) = place_from_operand(op) {
                if place_starts_with_deref(&p) {
                    let ptr_local = p.local;
                    if tainted_value_locals.contains(&ptr_local)
                        && is_raw_pointer_ty(body.local_decls[ptr_local].ty)
                    {
                        return UnsafeArgSummary::DIRECT_RAW_DEREF;
                    }
                }
            }
        }
        if rvalue_is_unsafe_root(body, rhs) && rvalue_tainted(tcx, body, rhs, tainted_value_locals)
        {
            return match rhs {
                Rvalue::RawPtr(..) => UnsafeArgSummary::DIRECT_RAW_CREATION,
                Rvalue::Cast(CastKind::PointerWithExposedProvenance, ..)
                | Rvalue::Cast(CastKind::Transmute, ..) => UnsafeArgSummary::DIRECT_PROVENANCE_CAST,
                _ => 0,
            };
        }
    }
    0
}

fn summarize_arg_effects<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    arg_local: Local,
    arg_index: usize,
) -> UnsafeArgSummary {
    let mut tainted_ptr_locals: HashSet<Local> = HashSet::from([arg_local]);
    let mut tainted_value_locals: HashSet<Local> = HashSet::from([arg_local]);

    let mut changed = true;
    while changed {
        changed = false;
        for block_data in body.basic_blocks.iter() {
            changed |= apply_block(
                tcx,
                body,
                block_data,
                &mut tainted_value_locals,
                &mut tainted_ptr_locals,
            );
        }
    }

    let mut direct_sink_mask = 0u32;
    let mut propagation_mask = 0u32;

    for block_data in body.basic_blocks.iter() {
        for stmt in block_data.statements.iter() {
            direct_sink_mask |= stmt_direct_sink_mask(tcx, body, stmt, &tainted_value_locals);
        }
        if let Some(term) = block_data.terminator.as_ref() {
            if let TerminatorKind::Call { func, args, .. } = &term.kind {
                let mut any_tainted_ptr_arg = false;
                let mut any_tainted_raw_arg = false;
                for arg in args.iter() {
                    if let Some(p) = place_from_operand(&arg.node) {
                        let local = p.local;
                        let local_ty = body.local_decls[local].ty;
                        if is_pointer_ty(local_ty) && tainted_value_locals.contains(&local) {
                            any_tainted_ptr_arg = true;
                            any_tainted_raw_arg |= is_raw_pointer_ty(local_ty);
                        }
                    }
                }
                let callee_summary = callee_summary(tcx, body, func);
                let unknown_boundary =
                    !instrumented_call_boundary(tcx, body, func) && callee_summary.is_none();
                if unknown_boundary && any_tainted_ptr_arg {
                    propagation_mask |= UnsafeArgSummary::PROP_ESCAPE_UNKNOWN;
                }
                if any_tainted_raw_arg && (unknown_boundary || callee_summary.is_none()) {
                    direct_sink_mask |= UnsafeArgSummary::DIRECT_RAW_ARG_TO_CALL;
                }
                if let Some(summary) = &callee_summary {
                    for (call_arg_index, arg) in args.iter().enumerate() {
                        if let Some(p) = place_from_operand(&arg.node) {
                            let local = p.local;
                            if tainted_value_locals.contains(&local) {
                                if let Some(callee_arg) = summary
                                    .ptr_args()
                                    .iter()
                                    .find(|entry| entry.arg_index() == call_arg_index)
                                {
                                    direct_sink_mask |= callee_arg.direct_sink_mask();
                                    propagation_mask |= callee_arg.propagation_mask()
                                        & (UnsafeArgSummary::PROP_ESCAPE_UNKNOWN
                                            | UnsafeArgSummary::PROP_ESCAPE_UNKNOWN_INHERITED);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    UnsafeArgSummary {
        arg_index,
        direct_sink_mask,
        propagation_mask: propagation_mask
            | if tainted_value_locals.contains(&RETURN_PLACE) {
                UnsafeArgSummary::PROP_FORWARD_TO_RETURN
            } else {
                0
            },
    }
}

fn compute_function_summary<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    tainted_value_locals: &HashSet<Local>,
) -> UnsafeFunctionSummary {
    let mut summary = UnsafeFunctionSummary::default();

    for block_data in body.basic_blocks.iter() {
        for stmt in block_data.statements.iter() {
            summary.has_direct_sink |=
                stmt_direct_sink_mask(tcx, body, stmt, tainted_value_locals) != 0;
        }
        if let Some(term) = block_data.terminator.as_ref() {
            if let TerminatorKind::Call { func, args, .. } = &term.kind {
                let callee_summary = callee_summary(tcx, body, func);
                let unknown_boundary =
                    !instrumented_call_boundary(tcx, body, func) && callee_summary.is_none();
                summary.calls_unknown_boundary_direct |= unknown_boundary
                    || (resolve_callee_def_id(tcx, body, func)
                        .is_some_and(|did| did.krate != LOCAL_CRATE)
                        && callee_summary
                            .as_ref()
                            .is_some_and(|callee| callee.calls_unknown_boundary()));
                summary.has_direct_sink |= (unknown_boundary || callee_summary.is_none())
                    && args.iter().any(|arg| {
                        place_from_operand(&arg.node).is_some_and(|p| {
                            let local = p.local;
                            is_raw_pointer_ty(body.local_decls[local].ty)
                                && tainted_value_locals.contains(&local)
                        })
                    });
                if let Some(callee) = &callee_summary {
                    summary.has_direct_sink |= args.iter().enumerate().any(|(arg_index, arg)| {
                        place_from_operand(&arg.node).is_some_and(|p| {
                            let local = p.local;
                            tainted_value_locals.contains(&local)
                                && callee.ptr_args().iter().any(|entry| {
                                    entry.arg_index() == arg_index && entry.reaches_direct_sink()
                                })
                        })
                    });
                }
            }
        }
    }

    for (arg_index, arg_local) in body.args_iter().enumerate() {
        if is_pointer_ty(body.local_decls[arg_local].ty) {
            summary
                .ptr_args
                .push(summarize_arg_effects(tcx, body, arg_local, arg_index));
        }
    }

    summary
}

fn collect_callsites<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> Vec<UnsafeCallsiteSummary> {
    let arg_index_by_local: HashMap<Local, usize> = body
        .args_iter()
        .enumerate()
        .map(|(arg_index, local)| (local, arg_index))
        .collect();
    let (forward_copy_edges, arg_origins) =
        compute_local_copy_flows(tcx, body, &arg_index_by_local);

    let mut out = Vec::new();
    for block_data in body.basic_blocks.iter() {
        let Some(term) = block_data.terminator.as_ref() else {
            continue;
        };
        let TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } = &term.kind
        else {
            continue;
        };
        let Some(did) = resolve_callee_def_id(tcx, body, func) else {
            continue;
        };
        if !instrumented_call_boundary(tcx, body, func) {
            continue;
        }

        let mut arg_edges = Vec::new();
        for (callee_arg_index, arg) in args.iter().enumerate() {
            let Some(place) = place_from_operand(&arg.node) else {
                continue;
            };
            if !is_pointer_ty(body.local_decls[place.local].ty) {
                continue;
            }
            if let Some(origins) = arg_origins.get(&place.local) {
                for caller_arg_index in origins.iter().copied() {
                    arg_edges.push(UnsafeArgEdge {
                        caller_arg_index,
                        callee_arg_index,
                    });
                }
            }
        }
        arg_edges.sort_by_key(|edge| (edge.caller_arg_index, edge.callee_arg_index));
        arg_edges.dedup_by_key(|edge| (edge.caller_arg_index, edge.callee_arg_index));

        let return_to_return = destination.local == RETURN_PLACE
            || local_flows_to_return(destination.local, &forward_copy_edges);

        if arg_edges.is_empty() && !return_to_return {
            continue;
        }

        out.push(UnsafeCallsiteSummary {
            callee_crate_name: tcx.crate_name(did.krate).as_str().to_string(),
            callee_function: tcx.def_path_str(did),
            callee_function_hash: def_path_hash_key(tcx, did),
            arg_edges,
            return_to_return,
        });
    }

    out
}

fn compute_local_copy_flows<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    arg_index_by_local: &HashMap<Local, usize>,
) -> (
    HashMap<Local, HashSet<Local>>,
    HashMap<Local, HashSet<usize>>,
) {
    let mut forward_copy_edges: HashMap<Local, HashSet<Local>> = HashMap::new();
    for block_data in body.basic_blocks.iter() {
        for stmt in block_data.statements.iter() {
            let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind else {
                continue;
            };
            let Some(dst_local) = lhs_place.as_local() else {
                continue;
            };
            if !is_pointer_ty(body.local_decls[dst_local].ty) {
                continue;
            }
            let Some(src_local) = simple_local_copy_source(rhs) else {
                continue;
            };
            if !is_pointer_ty(body.local_decls[src_local].ty) {
                continue;
            }
            forward_copy_edges
                .entry(src_local)
                .or_default()
                .insert(dst_local);
        }
    }

    let mut arg_origins: HashMap<Local, HashSet<usize>> = HashMap::new();
    let mut worklist: VecDeque<(Local, usize)> = VecDeque::new();
    for (&arg_local, &arg_index) in arg_index_by_local.iter() {
        arg_origins.entry(arg_local).or_default().insert(arg_index);
        worklist.push_back((arg_local, arg_index));
    }

    while let Some((local, arg_index)) = worklist.pop_front() {
        let Some(nexts) = forward_copy_edges.get(&local) else {
            continue;
        };
        for &next in nexts {
            let inserted = arg_origins.entry(next).or_default().insert(arg_index);
            if inserted {
                worklist.push_back((next, arg_index));
            }
        }
    }

    (forward_copy_edges, arg_origins)
}

fn local_flows_to_return(
    start: Local,
    forward_copy_edges: &HashMap<Local, HashSet<Local>>,
) -> bool {
    if start == RETURN_PLACE {
        return true;
    }
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start]);
    while let Some(local) = queue.pop_front() {
        if !seen.insert(local) {
            continue;
        }
        let Some(nexts) = forward_copy_edges.get(&local) else {
            continue;
        };
        for &next in nexts {
            if next == RETURN_PLACE {
                return true;
            }
            queue.push_back(next);
        }
    }
    false
}

pub(crate) fn compute_unsafe_influence<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    enabled: bool,
) -> UnsafeInfluence {
    if !enabled {
        return UnsafeInfluence::disabled();
    }

    let (tainted_ptr_locals, tainted_value_locals, total_ptr_locals, call_stats) =
        compute_tainted_state(tcx, body);

    UnsafeInfluence {
        enabled: true,
        tainted_ptr_locals,
        total_ptr_locals,
        summary: compute_function_summary(tcx, body, &tainted_value_locals),
        local_callsites: collect_callsites(tcx, body),
        call_stats,
    }
}

fn compute_tainted_state<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
) -> (
    HashSet<Local>,
    HashSet<Local>,
    usize,
    UnsafeCallRelevanceStats,
) {
    let mut relevant_ptr_locals: HashSet<Local> = HashSet::new();
    let mut relevant_value_locals: HashSet<Local> = HashSet::new();
    let mut total_ptr_locals = 0usize;
    let mut call_stats = UnsafeCallRelevanceStats::default();

    for local in body.local_decls.indices() {
        let ty = body.local_decls[local].ty;
        if is_pointer_ty(ty) {
            total_ptr_locals += 1;
        }
    }

    seed_sink_relevance(
        tcx,
        body,
        &mut relevant_value_locals,
        &mut relevant_ptr_locals,
        &mut call_stats,
    );

    // Monotonic backward reachability: mark values/ptrs that can flow into unsafe-sensitive sinks.
    let mut changed = true;
    while changed {
        changed = false;
        for block_data in body.basic_blocks.iter().rev() {
            changed |= backward_apply_block(
                tcx,
                body,
                block_data,
                &mut relevant_value_locals,
                &mut relevant_ptr_locals,
                &mut call_stats,
            );
        }
    }

    (
        relevant_ptr_locals,
        relevant_value_locals,
        total_ptr_locals,
        call_stats,
    )
}

fn apply_block<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    block_data: &BasicBlockData<'tcx>,
    tainted_value_locals: &mut HashSet<Local>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;
    for stmt in block_data.statements.iter() {
        changed |= apply_statement(tcx, body, stmt, tainted_value_locals, tainted_ptr_locals);
    }
    if let Some(term) = block_data.terminator.as_ref() {
        changed |= apply_terminator(tcx, body, term, tainted_value_locals, tainted_ptr_locals);
    }
    changed
}

fn seed_sink_relevance<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    relevant_value_locals: &mut HashSet<Local>,
    relevant_ptr_locals: &mut HashSet<Local>,
    call_stats: &mut UnsafeCallRelevanceStats,
) {
    for block_data in body.basic_blocks.iter() {
        for stmt in block_data.statements.iter() {
            if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
                if place_starts_with_deref(lhs_place) {
                    let ptr_local = lhs_place.local;
                    if is_raw_pointer_ty(body.local_decls[ptr_local].ty) {
                        mark_value_local(ptr_local, relevant_value_locals);
                        mark_ptr_local(ptr_local, relevant_ptr_locals);
                    }
                }
                if let Rvalue::Use(op) = rhs {
                    if let Some(p) = place_from_operand(op) {
                        if place_starts_with_deref(&p) {
                            let ptr_local = p.local;
                            if is_raw_pointer_ty(body.local_decls[ptr_local].ty) {
                                mark_value_local(ptr_local, relevant_value_locals);
                                mark_ptr_local(ptr_local, relevant_ptr_locals);
                            }
                        }
                    }
                }
            }
        }
        if let Some(term) = block_data.terminator.as_ref() {
            seed_terminator_sink_relevance(
                tcx,
                body,
                term,
                relevant_value_locals,
                relevant_ptr_locals,
                call_stats,
            );
        }
    }
}

fn seed_terminator_sink_relevance<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    term: &Terminator<'tcx>,
    relevant_value_locals: &mut HashSet<Local>,
    relevant_ptr_locals: &mut HashSet<Local>,
    call_stats: &mut UnsafeCallRelevanceStats,
) {
    let TerminatorKind::Call {
        func,
        args,
        destination,
        ..
    } = &term.kind
    else {
        return;
    };

    let callee_summary = callee_summary(tcx, body, func);
    let unknown_boundary = !instrumented_call_boundary(tcx, body, func) && callee_summary.is_none();
    let local_summary_missing = resolve_callee_def_id(tcx, body, func)
        .is_some_and(|did| did.krate == LOCAL_CRATE)
        && callee_summary.is_none();
    if local_summary_missing {
        trace_local_summary_missing(tcx, body, func);
    }

    for (arg_index, arg) in args.iter().enumerate() {
        let Some(place) = place_from_operand(&arg.node) else {
            continue;
        };
        let local = place.local;
        let ty = body.local_decls[local].ty;
        if !is_pointer_ty(ty) {
            continue;
        }
        let is_raw = is_raw_pointer_ty(ty);
        let sink_relevant = if unknown_boundary {
            call_stats.seed_arg_unknown_boundary += 1;
            record_unknown_callee_seed(tcx, body, func, call_stats);
            true
        } else if local_summary_missing {
            call_stats.seed_arg_local_summary_missing += 1;
            true
        } else if let Some(summary) = callee_summary.as_ref() {
            summary
                .ptr_args()
                .iter()
                .find(|entry| entry.arg_index() == arg_index)
                .is_some_and(|entry| {
                    let mut relevant = false;
                    if entry.reaches_direct_sink() {
                        call_stats.seed_arg_summary_direct_sink += 1;
                        relevant = true;
                    }
                    if entry.escapes_to_direct_unknown_boundary() {
                        call_stats.seed_arg_summary_escape_unknown_direct += 1;
                        relevant = true;
                    }
                    if entry.escapes_to_inherited_unknown_boundary() {
                        call_stats.seed_arg_summary_escape_unknown_inherited += 1;
                        relevant = true;
                    }
                    relevant
                })
        } else {
            // If an instrumented non-local callee has no summary available, keep raw-pointer
            // arguments conservative. Once a summary exists, only actual sink/escape effects
            // should keep the caller local relevant.
            if is_raw {
                call_stats.seed_arg_raw_fallback += 1;
            }
            is_raw
        };
        if sink_relevant {
            mark_value_local(local, relevant_value_locals);
            mark_ptr_local(local, relevant_ptr_locals);
        }
    }

    let _ = destination;
}

fn backward_apply_block<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    block_data: &BasicBlockData<'tcx>,
    relevant_value_locals: &mut HashSet<Local>,
    relevant_ptr_locals: &mut HashSet<Local>,
    call_stats: &mut UnsafeCallRelevanceStats,
) -> bool {
    let mut changed = false;
    if let Some(term) = block_data.terminator.as_ref() {
        changed |= backward_apply_terminator(
            tcx,
            body,
            term,
            relevant_value_locals,
            relevant_ptr_locals,
            call_stats,
        );
    }
    for stmt in block_data.statements.iter().rev() {
        changed |=
            backward_apply_statement(tcx, body, stmt, relevant_value_locals, relevant_ptr_locals);
    }
    changed
}

fn backward_apply_statement<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    stmt: &Statement<'tcx>,
    relevant_value_locals: &mut HashSet<Local>,
    relevant_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;
    let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind else {
        return false;
    };

    let lhs_local_relevant = relevant_value_locals.contains(&lhs_place.local);
    let projected_lhs_relevant = !lhs_place.projection.is_empty() && lhs_local_relevant;
    if !(lhs_local_relevant || projected_lhs_relevant) {
        return false;
    }

    match rhs {
        Rvalue::Use(op)
        | Rvalue::Repeat(op, _)
        | Rvalue::Cast(_, op, _)
        | Rvalue::UnaryOp(_, op) => {
            if let Some(place) = place_from_operand(op) {
                changed |= mark_value_local(place.local, relevant_value_locals);
                if is_pointer_ty(body.local_decls[place.local].ty) {
                    changed |= mark_ptr_local(place.local, relevant_ptr_locals);
                }
            }
        }
        Rvalue::RawPtr(_, place) | Rvalue::Ref(_, _, place) | Rvalue::CopyForDeref(place) => {
            changed |= mark_value_local(place.local, relevant_value_locals);
            if is_pointer_ty(body.local_decls[place.local].ty) {
                changed |= mark_ptr_local(place.local, relevant_ptr_locals);
            }
        }
        Rvalue::BinaryOp(_, ops) => {
            for op in [&ops.0, &ops.1] {
                if let Some(place) = place_from_operand(op) {
                    changed |= mark_value_local(place.local, relevant_value_locals);
                    if is_pointer_ty(body.local_decls[place.local].ty) {
                        changed |= mark_ptr_local(place.local, relevant_ptr_locals);
                    }
                }
            }
        }
        Rvalue::Aggregate(_, ops) => {
            for op in ops {
                if let Some(place) = place_from_operand(op) {
                    changed |= mark_value_local(place.local, relevant_value_locals);
                    if is_pointer_ty(body.local_decls[place.local].ty) {
                        changed |= mark_ptr_local(place.local, relevant_ptr_locals);
                    }
                }
            }
        }
        _ => {}
    }

    let _ = tcx;
    changed
}

fn backward_apply_terminator<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    term: &Terminator<'tcx>,
    relevant_value_locals: &mut HashSet<Local>,
    relevant_ptr_locals: &mut HashSet<Local>,
    call_stats: &mut UnsafeCallRelevanceStats,
) -> bool {
    let mut changed = false;
    let TerminatorKind::Call {
        func,
        args,
        destination,
        ..
    } = &term.kind
    else {
        return false;
    };

    if let Some(dst_local) = destination.as_local() {
        if relevant_value_locals.contains(&dst_local) {
            let callee_summary = callee_summary(tcx, body, func);
            let unknown_boundary =
                !instrumented_call_boundary(tcx, body, func) && callee_summary.is_none();
            let conservative_local_fallback = resolve_callee_def_id(tcx, body, func)
                .is_some_and(|did| did.krate == LOCAL_CRATE)
                && callee_summary.is_none();
            if conservative_local_fallback {
                trace_local_summary_missing(tcx, body, func);
            }
            if unknown_boundary || conservative_local_fallback {
                if unknown_boundary {
                    call_stats.backward_dst_unknown_boundary += 1;
                    record_unknown_callee_backward(tcx, body, func, call_stats);
                } else {
                    call_stats.backward_dst_local_summary_missing += 1;
                }
                for arg in args.iter() {
                    if let Some(place) = place_from_operand(&arg.node) {
                        if is_pointer_ty(body.local_decls[place.local].ty) {
                            changed |= mark_value_local(place.local, relevant_value_locals);
                            changed |= mark_ptr_local(place.local, relevant_ptr_locals);
                        }
                    }
                }
            } else if let Some(summary) = callee_summary {
                for (arg_index, arg) in args.iter().enumerate() {
                    if let Some(place) = place_from_operand(&arg.node) {
                        if !is_pointer_ty(body.local_decls[place.local].ty) {
                            continue;
                        }
                        if summary.ptr_args().iter().any(|entry| {
                            entry.arg_index() == arg_index && entry.forwarded_to_return()
                        }) {
                            call_stats.backward_dst_forward_to_return += 1;
                            changed |= mark_value_local(place.local, relevant_value_locals);
                            changed |= mark_ptr_local(place.local, relevant_ptr_locals);
                        }
                    }
                }
            }
        }
    }

    changed
}
