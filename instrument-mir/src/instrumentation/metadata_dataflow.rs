use std::collections::HashMap;

use rustc_middle::mir::{BasicBlock, Body, Local, START_BLOCK};

use super::{InsertPoint, InstrKind, MyOptimizationPass, PassLogLevel};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum MetaState {
    Zero,
    Fresh(MetaSymbol),
    Unknown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum MetaSymbol {
    InsertPoint(usize),
}

#[derive(Default)]
struct MetadataDflowStats {
    tag_props_before: usize,
    tag_props_after: usize,
}

pub(super) fn apply_metadata_dataflow<'tcx>(
    pass: &MyOptimizationPass,
    body: &Body<'tcx>,
    insert_points: &mut Vec<InsertPoint<'tcx>>,
) {
    if !metadata_dataflow_enabled() {
        return;
    }

    let analysis = MetadataAnalysis::new(pass, body, insert_points);
    let mut stats = MetadataDflowStats::default();
    stats.tag_props_before = insert_points
        .iter()
        .filter(|ip| matches!(ip.kind, InstrKind::TagProp { .. }))
        .count();

    let mut kept = Vec::with_capacity(insert_points.len());
    for (idx, ip) in std::mem::take(insert_points).into_iter().enumerate() {
        let redundant = match ip.kind {
            InstrKind::TagProp { dst, src } => analysis.is_redundant_tag_prop(idx, dst, src),
            _ => false,
        };
        if !redundant {
            kept.push(ip);
        }
    }
    *insert_points = kept;

    stats.tag_props_after = insert_points
        .iter()
        .filter(|ip| matches!(ip.kind, InstrKind::TagProp { .. }))
        .count();

    if metadata_dataflow_stats_enabled() || pass.log_enabled(PassLogLevel::Info) {
        let dropped = stats.tag_props_before.saturating_sub(stats.tag_props_after);
        if dropped != 0 {
            eprintln!(
                "[rusteze][meta-dflow] tag_props {}->{} dropped={}",
                stats.tag_props_before,
                stats.tag_props_after,
                dropped
            );
        }
    }
}

fn metadata_dataflow_enabled() -> bool {
    std::env::var("RZ_METADATA_DATAFLOW")
        .ok()
        .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn metadata_dataflow_stats_enabled() -> bool {
    std::env::var("RZ_METADATA_DATAFLOW_STATS")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

struct MetadataAnalysis {
    point_states: HashMap<usize, HashMap<Local, MetaState>>,
}

impl MetadataAnalysis {
    fn new<'tcx>(
        pass: &MyOptimizationPass,
        body: &Body<'tcx>,
        insert_points: &[InsertPoint<'tcx>],
    ) -> Self {
        let pointer_locals: Vec<Local> = body
            .local_decls
            .indices()
            .filter(|local| pass.is_pointer_ty(body.local_decls[*local].ty))
            .collect();

        let ordered_points = ordered_insert_points(insert_points);
        let predecessors = block_predecessors(body);

        let zero_state = pointer_locals
            .iter()
            .copied()
            .map(|local| (local, MetaState::Zero))
            .collect::<HashMap<_, _>>();

        let mut in_states: HashMap<BasicBlock, HashMap<Local, MetaState>> = HashMap::new();
        let mut out_states: HashMap<BasicBlock, HashMap<Local, MetaState>> = HashMap::new();
        in_states.insert(START_BLOCK, zero_state.clone());

        let mut worklist: Vec<BasicBlock> = body.basic_blocks.indices().collect();
        while let Some(bb) = worklist.pop() {
            let in_state = if bb == START_BLOCK {
                in_states.get(&bb).cloned().unwrap_or_else(|| zero_state.clone())
            } else {
                merge_block_states(&pointer_locals, predecessors.get(&bb), &out_states)
            };

            let out_state = transfer_block(&ordered_points, bb, &in_state);
            let changed_in = in_states.get(&bb) != Some(&in_state);
            let changed_out = out_states.get(&bb) != Some(&out_state);
            if changed_in {
                in_states.insert(bb, in_state);
            }
            if changed_out {
                out_states.insert(bb, out_state);
                if let Some(term) = &body.basic_blocks[bb].terminator {
                    for succ in term.successors() {
                        worklist.push(succ);
                    }
                }
            }
        }

        let mut point_states = HashMap::new();
        for (bb, points) in ordered_points.iter() {
            let mut state = in_states.get(bb).cloned().unwrap_or_else(|| zero_state.clone());
            for point in points {
                point_states.insert(point.idx, state.clone());
                apply_insert_point(point.idx, &point.kind, point.place_local, &mut state);
            }
        }

        Self { point_states }
    }

    fn is_redundant_tag_prop(&self, idx: usize, dst: Local, src: Local) -> bool {
        if dst == src {
            return true;
        }
        let Some(state) = self.point_states.get(&idx) else {
            return false;
        };
        state.get(&dst).copied() == state.get(&src).copied()
    }
}

#[derive(Clone)]
struct OrderedPoint<'tcx> {
    idx: usize,
    kind: InstrKind<'tcx>,
    place_local: Option<Local>,
}

fn ordered_insert_points<'tcx>(
    insert_points: &[InsertPoint<'tcx>],
) -> HashMap<BasicBlock, Vec<OrderedPoint<'tcx>>> {
    let mut indexed: Vec<(usize, &InsertPoint<'tcx>)> = insert_points.iter().enumerate().collect();
    indexed.sort_by_key(|(idx, ip)| {
        (
            ip.bb.index(),
            ip.stmt_idx,
            if ip.insert_before { 0_u8 } else { 1_u8 },
            instr_priority(&ip.kind),
            *idx,
        )
    });

    let mut per_block: HashMap<BasicBlock, Vec<OrderedPoint<'tcx>>> = HashMap::new();
    for (idx, ip) in indexed {
        per_block.entry(ip.bb).or_default().push(OrderedPoint {
            idx,
            kind: ip.kind.clone(),
            place_local: ip.place.as_local(),
        });
    }
    per_block
}

fn block_predecessors<'tcx>(body: &Body<'tcx>) -> HashMap<BasicBlock, Vec<BasicBlock>> {
    let mut predecessors: HashMap<BasicBlock, Vec<BasicBlock>> = HashMap::new();
    for (bb, data) in body.basic_blocks.iter_enumerated() {
        if let Some(term) = &data.terminator {
            for succ in term.successors() {
                predecessors.entry(succ).or_default().push(bb);
            }
        }
    }
    predecessors
}

fn merge_block_states(
    pointer_locals: &[Local],
    preds: Option<&Vec<BasicBlock>>,
    out_states: &HashMap<BasicBlock, HashMap<Local, MetaState>>,
) -> HashMap<Local, MetaState> {
    let Some(preds) = preds else {
        return HashMap::new();
    };
    if preds.is_empty() {
        return HashMap::new();
    }

    let mut merged = HashMap::new();
    for local in pointer_locals.iter().copied() {
        let first = preds
            .first()
            .and_then(|pred| out_states.get(pred))
            .and_then(|state| state.get(&local).copied())
            .unwrap_or(MetaState::Unknown);
        let same = preds.iter().skip(1).all(|pred| {
            out_states
                .get(pred)
                .and_then(|state| state.get(&local).copied())
                .unwrap_or(MetaState::Unknown)
                == first
        });
        merged.insert(local, if same { first } else { MetaState::Unknown });
    }
    merged
}

fn transfer_block<'tcx>(
    ordered_points: &HashMap<BasicBlock, Vec<OrderedPoint<'tcx>>>,
    bb: BasicBlock,
    in_state: &HashMap<Local, MetaState>,
) -> HashMap<Local, MetaState> {
        let mut state = in_state.clone();
    if let Some(points) = ordered_points.get(&bb) {
        for point in points {
            apply_insert_point(point.idx, &point.kind, point.place_local, &mut state);
        }
    }
    state
}

fn apply_insert_point(
    idx: usize,
    kind: &InstrKind<'_>,
    place_local: Option<Local>,
    state: &mut HashMap<Local, MetaState>,
) {
    match *kind {
        InstrKind::TagProp { dst, src } => {
            let src_state = state.get(&src).copied().unwrap_or(MetaState::Unknown);
            state.insert(dst, src_state);
        }
        InstrKind::Ref { .. } | InstrKind::Raw { .. } => {
            if let Some(dst) = place_local {
                state.insert(dst, MetaState::Fresh(MetaSymbol::InsertPoint(idx)));
            }
        }
        InstrKind::RawRoot { ptr_local: dst, .. }
        | InstrKind::ArgRetag { ptr_local: dst, .. }
        | InstrKind::RetTake { dst_local: dst, .. }
        | InstrKind::RetRoot { dst_local: dst, .. }
        | InstrKind::PtrDerive { dst, .. } => {
            state.insert(dst, MetaState::Fresh(MetaSymbol::InsertPoint(idx)));
        }
        _ => {}
    }
}

fn instr_priority(kind: &InstrKind<'_>) -> u8 {
    match kind {
        InstrKind::Ref { .. }
        | InstrKind::Raw { .. }
        | InstrKind::RawRoot { .. }
        | InstrKind::ArgRetag { .. }
        | InstrKind::FnExit { .. }
        | InstrKind::RetRoot { .. }
        | InstrKind::PtrDerive { .. } => 0,
        InstrKind::PtrRead { .. }
        | InstrKind::PtrWrite { .. }
        | InstrKind::PtrReadAllowUntagged { .. }
        | InstrKind::PtrWriteAllowUntagged { .. } => 1,
        InstrKind::CallArgPush { .. } | InstrKind::PtrUse { .. } => 2,
        _ => 3,
    }
}
