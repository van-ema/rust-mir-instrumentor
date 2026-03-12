use std::collections::HashSet;
use std::sync::OnceLock;

use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_middle::mir::{
    BasicBlockData, Body, CastKind, Local, Operand, Place, ProjectionElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, RETURN_PLACE,
};
use rustc_middle::ty::{TyCtxt, TyKind};

#[derive(Clone, Debug)]
pub(crate) struct UnsafeInfluence {
    enabled: bool,
    tainted_ptr_locals: HashSet<Local>,
    total_ptr_locals: usize,
    summary: UnsafeFunctionSummary,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnsafeFunctionSummary {
    has_direct_sink: bool,
    calls_unknown_boundary: bool,
    ptr_args: Vec<UnsafeArgSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnsafeArgSummary {
    pub(crate) arg_index: usize,
    pub(crate) direct_sink_mask: u32,
    pub(crate) propagation_mask: u32,
}


impl UnsafeInfluence {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            tainted_ptr_locals: HashSet::new(),
            total_ptr_locals: 0,
            summary: UnsafeFunctionSummary::default(),
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
}

impl UnsafeFunctionSummary {
    pub(crate) fn has_direct_sink(&self) -> bool {
        self.has_direct_sink
    }

    pub(crate) fn calls_unknown_boundary(&self) -> bool {
        self.calls_unknown_boundary
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

    pub(crate) fn reaches_direct_sink(&self) -> bool {
        self.direct_sink_mask != 0
    }

    pub(crate) fn escapes_to_unknown_boundary(&self) -> bool {
        (self.propagation_mask & Self::PROP_ESCAPE_UNKNOWN) != 0
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

pub(crate) fn unsafe_dataflow_enabled() -> bool {
    std::env::var("RZ_UNSAFE_DATAFLOW")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
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

fn taint_local(local: Local, tainted_ptr_locals: &mut HashSet<Local>) -> bool {
    tainted_ptr_locals.insert(local)
}

fn taint_value_local(local: Local, tainted_value_locals: &mut HashSet<Local>) -> bool {
    tainted_value_locals.insert(local)
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
            tainted_value_locals.contains(&p.local) || is_pointer_ty(p.ty(&body.local_decls, tcx).ty)
        }
        Rvalue::Ref(_, _, p) | Rvalue::CopyForDeref(p) => tainted_value_locals.contains(&p.local),
        Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => operand_tainted(op, tainted_value_locals),
        Rvalue::BinaryOp(_, ops) => {
            operand_tainted(&ops.0, tainted_value_locals)
                || operand_tainted(&ops.1, tainted_value_locals)
        }
        Rvalue::Aggregate(_, ops) => ops.iter().any(|op| operand_tainted(op, tainted_value_locals)),
        _ => false,
    }
}

fn rvalue_is_unsafe_root<'tcx>(
    body: &Body<'tcx>,
    rv: &Rvalue<'tcx>,
) -> bool {
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
    match func {
        Operand::Constant(c) => match c.const_.ty().kind() {
            TyKind::FnDef(def_id, _) => Some(*def_id),
            _ => None,
        },
        Operand::Copy(p) | Operand::Move(p) => match p.ty(&body.local_decls, tcx).ty.kind() {
            TyKind::FnDef(def_id, _) => Some(*def_id),
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
            || p.ends_with("::deref")
            || p.ends_with("::deref_mut")
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

fn callee_summary<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> Option<UnsafeFunctionSummary> {
    let did = resolve_callee_def_id(tcx, body, func)?;
    if did.krate == LOCAL_CRATE {
        None
    } else {
        known_external_summary(tcx, did)
    }
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
        let mut any_tainted_arg = false;

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
                    any_tainted_arg |= tainted_value_locals.contains(&local);
                }
            }
        }

        // Conservative boundary: treat only unresolved or intentionally-uninstrumented callees
        // (std/core/runtime/uninstrumented deps) as unknown. Cross-crate calls to instrumented
        // dependencies should not taint by default.
        let unknown_boundary = !instrumented_call_boundary(tcx, body, func);
        let callee_summary = callee_summary(tcx, body, func);
        let conservative_local_fallback = !unknown_boundary
            && resolve_callee_def_id(tcx, body, func)
                .is_some_and(|did| did.krate == LOCAL_CRATE)
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
                    if arg_summary.reaches_direct_sink() || arg_summary.escapes_to_unknown_boundary()
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
                    ptr_arg_locals.iter().any(|(arg_index, _)| {
                        summary
                            .ptr_args()
                            .iter()
                            .any(|entry| {
                                entry.arg_index() == *arg_index && entry.forwarded_to_return()
                            })
                    })
                });
                if unknown_boundary
                    || any_raw_arg
                    || any_tainted_arg
                    || local_returned_from_arg
                    || conservative_local_fallback
                {
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
        if rvalue_is_unsafe_root(body, rhs) && rvalue_tainted(tcx, body, rhs, tainted_value_locals) {
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
                let unknown_boundary = !instrumented_call_boundary(tcx, body, func);
                let callee_summary = callee_summary(tcx, body, func);
                if unknown_boundary && any_tainted_ptr_arg {
                    propagation_mask |= UnsafeArgSummary::PROP_ESCAPE_UNKNOWN;
                }
                if any_tainted_raw_arg {
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
                                        & UnsafeArgSummary::PROP_ESCAPE_UNKNOWN;
                                }
                            }
                        }
                    }
                } else if resolve_callee_def_id(tcx, body, func)
                    .is_some_and(|did| did.krate == LOCAL_CRATE)
                    && any_tainted_ptr_arg
                {
                    propagation_mask |= UnsafeArgSummary::PROP_ESCAPE_UNKNOWN;
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
                let unknown_boundary = !instrumented_call_boundary(tcx, body, func);
                let callee_summary = callee_summary(tcx, body, func);
                summary.calls_unknown_boundary |= unknown_boundary
                    || (resolve_callee_def_id(tcx, body, func)
                        .is_some_and(|did| did.krate == LOCAL_CRATE)
                        && callee_summary
                            .as_ref()
                            .is_some_and(|callee| callee.calls_unknown_boundary()));
                summary.has_direct_sink |= args.iter().any(|arg| {
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
                                && callee
                                    .ptr_args()
                                    .iter()
                                    .any(|entry| {
                                        entry.arg_index() == arg_index
                                            && entry.reaches_direct_sink()
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
                .push(summarize_arg_effects(
                    tcx,
                    body,
                    arg_local,
                    arg_index,
                ));
        }
    }

    summary
}

pub(crate) fn compute_unsafe_influence<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    enabled: bool,
) -> UnsafeInfluence {
    if !enabled {
        return UnsafeInfluence::disabled();
    }

    let (tainted_ptr_locals, tainted_value_locals, total_ptr_locals) =
        compute_tainted_state(tcx, body);

    UnsafeInfluence {
        enabled: true,
        tainted_ptr_locals,
        total_ptr_locals,
        summary: compute_function_summary(tcx, body, &tainted_value_locals),
    }
}

fn compute_tainted_state<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
) -> (HashSet<Local>, HashSet<Local>, usize) {
    let mut tainted_ptr_locals: HashSet<Local> = HashSet::new();
    let mut tainted_value_locals: HashSet<Local> = HashSet::new();
    let mut total_ptr_locals = 0usize;

    for local in body.local_decls.indices() {
        let ty = body.local_decls[local].ty;
        if is_pointer_ty(ty) {
            total_ptr_locals += 1;
            // Raw pointers are unsafe-influence roots by default.
            if is_raw_pointer_ty(ty) {
                tainted_ptr_locals.insert(local);
                tainted_value_locals.insert(local);
            }
        }
    }

    // Pointer arguments may be influenced by external callers.
    for arg_local in body.args_iter() {
        let arg_ty = body.local_decls[arg_local].ty;
        if is_pointer_ty(arg_ty) {
            tainted_ptr_locals.insert(arg_local);
            tainted_value_locals.insert(arg_local);
        }
    }

    // Monotonic fixed point over MIR: taint is only added, never removed.
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

    (tainted_ptr_locals, tainted_value_locals, total_ptr_locals)
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
        changed |= apply_terminator(
            tcx,
            body,
            term,
            tainted_value_locals,
            tainted_ptr_locals,
        );
    }
    changed
}
