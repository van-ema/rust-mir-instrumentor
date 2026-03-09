use std::collections::{HashMap, VecDeque};

use rustc_middle::mir::{
    BasicBlock, Body, BorrowKind, Local, Operand, Place, RawPtrKind, Rvalue, StatementKind,
    TerminatorKind, START_BLOCK,
};

use super::{InsertPoint, InstrKind, MyOptimizationPass, PassLogLevel};

// This pass is intentionally narrow:
// - it only reasons about pointer-local provenance equivalence
// - it only rewrites metadata-propagation hooks (`TagProp`, `PtrDerive`)
// - it does not suppress semantic hooks such as ref/raw creation or ptr reads/writes
//
// The guiding invariant is:
// if two pointer locals are proven to carry the same abstract provenance symbol at a program
// point, a later metadata consumer may read the tag from either local.
//
// We still break equivalence conservatively when a pointer local can be mutated through another
// alias, for example:
//   let p = q;
//   let r = &mut p;      // p's stored pointer value may change through r
// In that case later consumers must not be rewritten from `p` back to `q`.

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum ProvenanceValue {
    Unknown,
    Symbol(ProvenanceSymbol),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum ProvenanceSymbol {
    Entry(Local),
    Statement {
        bb: BasicBlock,
        stmt_idx: usize,
        local: Local,
    },
    Terminator {
        bb: BasicBlock,
        local: Local,
    },
}

#[derive(Default)]
struct ProvenanceStats {
    tag_props_before: usize,
    tag_props_after: usize,
    rewritten_uses: usize,
}

pub(super) fn apply_provenance_dataflow<'tcx>(
    pass: &MyOptimizationPass,
    body: &Body<'tcx>,
    insert_points: &mut Vec<InsertPoint<'tcx>>,
) -> HashMap<usize, Local> {
    if !provenance_dataflow_enabled() {
        return HashMap::new();
    }

    let analysis = ProvenanceAnalysis::new(pass, body);
    let mut stats = ProvenanceStats::default();
    stats.tag_props_before = insert_points
        .iter()
        .filter(|ip| matches!(ip.kind, InstrKind::TagProp { .. }))
        .count();

    let mut override_by_old_idx: HashMap<usize, Local> = HashMap::new();

    for (idx, ip) in insert_points.iter().enumerate() {
        if let Some(input_local) = natural_provenance_input_local(ip) {
            let point = analysis.point_value(ip, input_local);
            if let Some(rep_local) = analysis.representative_for(ip, input_local) {
                if rep_local != input_local && !matches!(point, ProvenanceValue::Unknown) {
                    override_by_old_idx.insert(idx, rep_local);
                }
            }
        }
    }

    let mut remapped_overrides: HashMap<usize, Local> = HashMap::new();
    let mut new_points: Vec<InsertPoint<'tcx>> = Vec::with_capacity(insert_points.len());

    for (old_idx, ip) in std::mem::take(insert_points).into_iter().enumerate() {
        let new_idx = new_points.len();
        if let Some(rep_local) = override_by_old_idx.get(&old_idx).copied() {
            remapped_overrides.insert(new_idx, rep_local);
            stats.rewritten_uses += 1;
        }
        new_points.push(ip);
    }

    *insert_points = new_points;
    stats.tag_props_after = insert_points
        .iter()
        .filter(|ip| matches!(ip.kind, InstrKind::TagProp { .. }))
        .count();

    if provenance_dataflow_stats_enabled() || pass.log_enabled(PassLogLevel::Info) {
        let dropped = stats.tag_props_before.saturating_sub(stats.tag_props_after);
        if dropped != 0 || stats.rewritten_uses != 0 {
            eprintln!(
                "[rusteze][prov-dflow] tag_props {}->{} dropped={} rewritten_uses={}",
                stats.tag_props_before,
                stats.tag_props_after,
                dropped,
                stats.rewritten_uses
            );
        }
    }

    remapped_overrides
}

fn provenance_dataflow_enabled() -> bool {
    // Experimental: pointer-value provenance equivalence alone is not enough to rewrite tag-local
    // consumers soundly. Keep the pass opt-in until it reasons about tag materialization directly.
    std::env::var("RZ_PROVENANCE_DATAFLOW")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn provenance_dataflow_stats_enabled() -> bool {
    std::env::var("RZ_PROVENANCE_DATAFLOW_STATS")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn place_from_operand<'tcx>(op: &Operand<'tcx>) -> Option<Place<'tcx>> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => Some(*p),
        _ => None,
    }
}

fn natural_provenance_input_local<'tcx>(ip: &InsertPoint<'tcx>) -> Option<Local> {
    match ip.kind {
        InstrKind::TagProp { src, .. } | InstrKind::PtrDerive { src, .. } => Some(src),
        _ => None,
    }
}

struct ProvenanceAnalysis<'a, 'tcx> {
    body: &'a Body<'tcx>,
    pointer_locals: Vec<Local>,
    stmt_states: HashMap<BasicBlock, Vec<HashMap<Local, ProvenanceValue>>>,
}

impl<'a, 'tcx> ProvenanceAnalysis<'a, 'tcx> {
    fn new(pass: &'a MyOptimizationPass, body: &'a Body<'tcx>) -> Self {
        let pointer_locals: Vec<Local> = body
            .local_decls
            .indices()
            .filter(|local| pass.is_pointer_ty(body.local_decls[*local].ty))
            .collect();

        let mut predecessors: HashMap<BasicBlock, Vec<BasicBlock>> = HashMap::new();
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            if let Some(term) = &data.terminator {
                for succ in term.successors() {
                    predecessors.entry(succ).or_default().push(bb);
                }
            }
        }

        let mut in_states: HashMap<BasicBlock, HashMap<Local, ProvenanceValue>> = HashMap::new();
        let mut out_states: HashMap<BasicBlock, HashMap<Local, ProvenanceValue>> = HashMap::new();

        let mut start_state = HashMap::new();
        for arg in body.args_iter() {
            if pass.is_pointer_ty(body.local_decls[arg].ty) {
                start_state.insert(arg, ProvenanceValue::Symbol(ProvenanceSymbol::Entry(arg)));
            }
        }
        in_states.insert(START_BLOCK, start_state);

        let mut worklist: VecDeque<BasicBlock> = body.basic_blocks.indices().collect();
        while let Some(bb) = worklist.pop_front() {
            let in_state = if bb == START_BLOCK {
                in_states.get(&bb).cloned().unwrap_or_default()
            } else {
                merge_predecessors(&pointer_locals, predecessors.get(&bb), &out_states)
            };

            let out_state = transfer_block(pass, body, bb, &in_state);
            let changed_in = in_states.get(&bb) != Some(&in_state);
            let changed_out = out_states.get(&bb) != Some(&out_state);
            if changed_in {
                in_states.insert(bb, in_state);
            }
            if changed_out {
                out_states.insert(bb, out_state);
                if let Some(term) = &body.basic_blocks[bb].terminator {
                    for succ in term.successors() {
                        worklist.push_back(succ);
                    }
                }
            }
        }

        let mut stmt_states = HashMap::new();
        for (bb, block) in body.basic_blocks.iter_enumerated() {
            let mut states = Vec::with_capacity(block.statements.len() + 1);
            let mut state = in_states.get(&bb).cloned().unwrap_or_default();
            states.push(state.clone());
            for (stmt_idx, stmt) in block.statements.iter().enumerate() {
                transfer_statement(pass, body, bb, stmt_idx, stmt.kind.clone(), &mut state);
                states.push(state.clone());
            }
            stmt_states.insert(bb, states);
        }

        Self {
            body,
            pointer_locals,
            stmt_states,
        }
    }

    fn point_state(&self, ip: &InsertPoint<'tcx>) -> &HashMap<Local, ProvenanceValue> {
        let states = self
            .stmt_states
            .get(&ip.bb)
            .expect("missing statement provenance state");
        let block = &self.body.basic_blocks[ip.bb];

        if ip.stmt_idx >= block.statements.len() {
            return states.last().expect("missing terminal provenance state");
        }

        if ip.insert_before {
            &states[ip.stmt_idx]
        } else {
            &states[ip.stmt_idx + 1]
        }
    }

    fn point_value(&self, ip: &InsertPoint<'tcx>, local: Local) -> ProvenanceValue {
        self.point_state(ip)
            .get(&local)
            .copied()
            .unwrap_or(ProvenanceValue::Unknown)
    }

    fn representative_for(&self, ip: &InsertPoint<'tcx>, local: Local) -> Option<Local> {
        let state = self.point_state(ip);
        let value = state.get(&local).copied().unwrap_or(ProvenanceValue::Unknown);
        let ProvenanceValue::Symbol(symbol) = value else {
            return None;
        };

        let mut best: Option<Local> = None;
        for candidate in self.pointer_locals.iter().copied() {
            if candidate == local {
                continue;
            }
            if state.get(&candidate).copied() == Some(ProvenanceValue::Symbol(symbol)) {
                best = match best {
                    Some(current) if current <= candidate => Some(current),
                    _ => Some(candidate),
                };
            }
        }

        best
    }
}

fn merge_predecessors(
    pointer_locals: &[Local],
    preds: Option<&Vec<BasicBlock>>,
    out_states: &HashMap<BasicBlock, HashMap<Local, ProvenanceValue>>,
) -> HashMap<Local, ProvenanceValue> {
    let Some(preds) = preds else {
        return HashMap::new();
    };
    if preds.is_empty() {
        return HashMap::new();
    }

    let mut merged = HashMap::new();
    for local in pointer_locals.iter().copied() {
        let mut candidate: Option<ProvenanceValue> = None;
        let mut same = true;
        for pred in preds.iter().copied() {
            let pred_value = out_states
                .get(&pred)
                .and_then(|state| state.get(&local).copied())
                .unwrap_or(ProvenanceValue::Unknown);
            if let Some(current) = candidate {
                if current != pred_value {
                    same = false;
                    break;
                }
            } else {
                candidate = Some(pred_value);
            }
        }

        if same {
            if let Some(ProvenanceValue::Symbol(symbol)) = candidate {
                merged.insert(local, ProvenanceValue::Symbol(symbol));
            }
        }
    }
    merged
}

fn transfer_block<'tcx>(
    pass: &MyOptimizationPass,
    body: &Body<'tcx>,
    bb: BasicBlock,
    in_state: &HashMap<Local, ProvenanceValue>,
) -> HashMap<Local, ProvenanceValue> {
    let mut state = in_state.clone();
    let block = &body.basic_blocks[bb];
    for (stmt_idx, stmt) in block.statements.iter().enumerate() {
        transfer_statement(pass, body, bb, stmt_idx, stmt.kind.clone(), &mut state);
    }

    if let Some(term) = &block.terminator {
        if let TerminatorKind::Call { destination, .. } = &term.kind {
            if let Some(dst_local) = destination.as_local() {
                if pass.is_pointer_ty(body.local_decls[dst_local].ty) {
                    state.insert(
                        dst_local,
                        ProvenanceValue::Symbol(ProvenanceSymbol::Terminator { bb, local: dst_local }),
                    );
                }
            }
        }
    }

    state
}

fn transfer_statement<'tcx>(
    pass: &MyOptimizationPass,
    body: &Body<'tcx>,
    bb: BasicBlock,
    stmt_idx: usize,
    kind: StatementKind<'tcx>,
    state: &mut HashMap<Local, ProvenanceValue>,
) {
    match kind {
        StatementKind::Assign(box (lhs_place, rhs)) => {
            if let Rvalue::Ref(_, borrow_kind, src_place) = &rhs {
                if let Some(src_local) = src_place.as_local() {
                    if pass.is_pointer_ty(body.local_decls[src_local].ty)
                        && matches!(borrow_kind, BorrowKind::Mut { .. })
                    {
                        state.insert(
                            src_local,
                            ProvenanceValue::Symbol(ProvenanceSymbol::Statement {
                                bb,
                                stmt_idx,
                                local: src_local,
                            }),
                        );
                    }
                }
            }

            if let Rvalue::RawPtr(mutbl, src_place) = &rhs {
                if let Some(src_local) = src_place.as_local() {
                    if pass.is_pointer_ty(body.local_decls[src_local].ty)
                        && matches!(mutbl, RawPtrKind::Mut)
                    {
                        state.insert(
                            src_local,
                            ProvenanceValue::Symbol(ProvenanceSymbol::Statement {
                                bb,
                                stmt_idx,
                                local: src_local,
                            }),
                        );
                    }
                }
            }

            let Some(dst_local) = lhs_place.as_local() else {
                return;
            };
            if !pass.is_pointer_ty(body.local_decls[dst_local].ty) {
                return;
            }

            if let Some(src_local) = simple_copy_source_local(pass, body, &rhs) {
                let value = state
                    .get(&src_local)
                    .copied()
                    .unwrap_or(ProvenanceValue::Unknown);
                match value {
                    ProvenanceValue::Symbol(symbol) => {
                        state.insert(dst_local, ProvenanceValue::Symbol(symbol));
                    }
                    ProvenanceValue::Unknown => {
                        state.remove(&dst_local);
                    }
                }
            } else {
                state.insert(
                    dst_local,
                    ProvenanceValue::Symbol(ProvenanceSymbol::Statement {
                        bb,
                        stmt_idx,
                        local: dst_local,
                    }),
                );
            }
        }
        StatementKind::StorageDead(local) => {
            state.remove(&local);
        }
        StatementKind::StorageLive(local) => {
            state.remove(&local);
        }
        _ => {}
    }
}

fn simple_copy_source_local<'tcx>(
    pass: &MyOptimizationPass,
    body: &Body<'tcx>,
    rhs: &Rvalue<'tcx>,
) -> Option<Local> {
    match rhs {
        Rvalue::Use(op) => place_from_operand(op)
            .filter(|p| p.projection.is_empty())
            .map(|p| p.local)
            .filter(|local| pass.is_pointer_ty(body.local_decls[*local].ty)),
        _ => None,
    }
}
