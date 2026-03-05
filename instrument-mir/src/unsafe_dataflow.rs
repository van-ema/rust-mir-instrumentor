use std::collections::HashSet;

use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::mir::{
    BasicBlockData, Body, Local, Operand, Place, ProjectionElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind,
};
use rustc_middle::ty::{TyCtxt, TyKind};

#[derive(Clone, Debug)]
pub(crate) struct UnsafeInfluence {
    enabled: bool,
    tainted_ptr_locals: HashSet<Local>,
    total_ptr_locals: usize,
}

impl UnsafeInfluence {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            tainted_ptr_locals: HashSet::new(),
            total_ptr_locals: 0,
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

fn operand_tainted<'tcx>(
    op: &Operand<'tcx>,
    tainted_ptr_locals: &HashSet<Local>,
) -> bool {
    place_from_operand(op)
        .map(|p| tainted_ptr_locals.contains(&p.local))
        .unwrap_or(false)
}

fn rvalue_tainted<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    rv: &Rvalue<'tcx>,
    tainted_ptr_locals: &HashSet<Local>,
) -> bool {
    match rv {
        Rvalue::Use(op) | Rvalue::Repeat(op, _) => operand_tainted(op, tainted_ptr_locals),
        Rvalue::RawPtr(_, p) => {
            // Raw pointer construction is an unsafe-influence root.
            tainted_ptr_locals.contains(&p.local) || is_pointer_ty(p.ty(&body.local_decls, tcx).ty)
        }
        Rvalue::Ref(_, _, p) | Rvalue::CopyForDeref(p) => tainted_ptr_locals.contains(&p.local),
        Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => operand_tainted(op, tainted_ptr_locals),
        Rvalue::BinaryOp(_, ops) => {
            operand_tainted(&ops.0, tainted_ptr_locals) || operand_tainted(&ops.1, tainted_ptr_locals)
        }
        Rvalue::Aggregate(_, ops) => ops.iter().any(|op| operand_tainted(op, tainted_ptr_locals)),
        _ => false,
    }
}

fn direct_local_callee<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
) -> bool {
    let def_id_opt = match func {
        Operand::Constant(c) => match c.const_.ty().kind() {
            TyKind::FnDef(def_id, _) => Some(*def_id),
            _ => None,
        },
        Operand::Copy(p) | Operand::Move(p) => match p.ty(&body.local_decls, tcx).ty.kind() {
            TyKind::FnDef(def_id, _) => Some(*def_id),
            _ => None,
        },
        _ => None,
    };
    def_id_opt.map_or(false, |did| did.krate == LOCAL_CRATE)
}

fn apply_statement<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    stmt: &Statement<'tcx>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;

    if let StatementKind::Assign(box (lhs_place, rhs)) = &stmt.kind {
        // Deref read/write roots through raw pointers.
        if place_starts_with_deref(lhs_place) {
            let ptr_local = lhs_place.local;
            let ptr_ty = body.local_decls[ptr_local].ty;
            if is_raw_pointer_ty(ptr_ty) {
                changed |= taint_local(ptr_local, tainted_ptr_locals);
            }
        }
        if let Rvalue::Use(op) = rhs {
            if let Some(p) = place_from_operand(op) {
                if place_starts_with_deref(&p) {
                    let ptr_local = p.local;
                    let ptr_ty = body.local_decls[ptr_local].ty;
                    if is_raw_pointer_ty(ptr_ty) {
                        changed |= taint_local(ptr_local, tainted_ptr_locals);
                    }
                }
            }
        }

        if let Some(dst_local) = lhs_place.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if is_pointer_ty(dst_ty) {
                let rhs_is_tainted = rvalue_tainted(tcx, body, rhs, tainted_ptr_locals);
                let rhs_is_unsafe_root = matches!(rhs, Rvalue::RawPtr(..));
                if rhs_is_tainted || rhs_is_unsafe_root {
                    changed |= taint_local(dst_local, tainted_ptr_locals);
                }
            }
        }
    }

    changed
}

fn apply_terminator<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    term: &Terminator<'tcx>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;

    if let TerminatorKind::Call { func, args, destination, .. } = &term.kind {
        let mut ptr_arg_locals: Vec<Local> = Vec::new();
        let mut any_raw_arg = false;
        let mut any_tainted_arg = false;

        for arg in args.iter() {
            if let Some(p) = place_from_operand(&arg.node) {
                let local = p.local;
                let local_ty = body.local_decls[local].ty;
                if is_pointer_ty(local_ty) {
                    ptr_arg_locals.push(local);
                    any_raw_arg |= is_raw_pointer_ty(local_ty);
                    any_tainted_arg |= tainted_ptr_locals.contains(&local);
                }
            }
        }

        // Crate-level conservative boundary: if callee is not a direct local fn, treat as unknown.
        let unknown_boundary = !direct_local_callee(tcx, body, func);

        if unknown_boundary || any_raw_arg {
            for local in ptr_arg_locals.iter().copied() {
                changed |= taint_local(local, tainted_ptr_locals);
            }
        }

        if let Some(dst_local) = destination.as_local() {
            let dst_ty = body.local_decls[dst_local].ty;
            if is_pointer_ty(dst_ty) && (unknown_boundary || any_raw_arg || any_tainted_arg) {
                changed |= taint_local(dst_local, tainted_ptr_locals);
            }
        }
    }

    changed
}

pub(crate) fn compute_unsafe_influence<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    enabled: bool,
) -> UnsafeInfluence {
    if !enabled {
        return UnsafeInfluence::disabled();
    }

    let mut tainted_ptr_locals: HashSet<Local> = HashSet::new();
    let mut total_ptr_locals = 0usize;

    for local in body.local_decls.indices() {
        let ty = body.local_decls[local].ty;
        if is_pointer_ty(ty) {
            total_ptr_locals += 1;
            // Raw pointers are unsafe-influence roots by default.
            if is_raw_pointer_ty(ty) {
                tainted_ptr_locals.insert(local);
            }
        }
    }

    // Pointer arguments may be influenced by external callers.
    for arg_local in body.args_iter() {
        let arg_ty = body.local_decls[arg_local].ty;
        if is_pointer_ty(arg_ty) {
            tainted_ptr_locals.insert(arg_local);
        }
    }

    // Monotonic fixed point over MIR: taint is only added, never removed.
    let mut changed = true;
    while changed {
        changed = false;
        for block_data in body.basic_blocks.iter() {
            changed |= apply_block(tcx, body, block_data, &mut tainted_ptr_locals);
        }
    }

    UnsafeInfluence {
        enabled: true,
        tainted_ptr_locals,
        total_ptr_locals,
    }
}

fn apply_block<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    block_data: &BasicBlockData<'tcx>,
    tainted_ptr_locals: &mut HashSet<Local>,
) -> bool {
    let mut changed = false;
    for stmt in block_data.statements.iter() {
        changed |= apply_statement(tcx, body, stmt, tainted_ptr_locals);
    }
    if let Some(term) = block_data.terminator.as_ref() {
        changed |= apply_terminator(tcx, body, term, tainted_ptr_locals);
    }
    changed
}
