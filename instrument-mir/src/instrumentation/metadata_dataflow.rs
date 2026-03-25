//! Metadata-dataflow optimization for pointer tag plumbing.
//!
//! This pass runs after instrumentation points have been collected and before
//! they are lowered into MIR statements/calls. Its job is narrow:
//! - track abstract tag/ref-ancestor metadata for pointer locals
//! - compute whether a scheduled `TagProp` is redundant
//! - drop redundant copies of tag and ref-ancestor metadata
//!
//! In other words, this is not safety analysis for the target program. It is an
//! optimization pass for Rusteze's own metadata propagation.
//!
//! The pass models only enough state to answer questions like:
//! - does `dst` already hold the same tag state as `src`?
//! - is `dst`'s tag or ref-ancestor ever read later by another hook?
//!
//! If the answer is "no", the corresponding `TagProp` field can be removed.
//! Missing optimization is acceptable; incorrect removal is not.
//!
//! Important caveat:
//! the ordering of same-site insert points matters. If metadata propagation is
//! analyzed as occurring after a consuming `PtrWrite`/`PtrRead`/`PtrUse`, the
//! pass can incorrectly conclude that the propagation is dead and delete it.
//! That turns concrete violations into `UNKNOWN_TAG` reports. When debugging
//! provenance loss, compare behavior with `RZ_METADATA_DATAFLOW=0`.
//!
//! Because that optimization is not yet proven sound in all cases, it is gated
//! behind an explicit opt-in flag today.

use std::collections::{HashMap, HashSet};

use rustc_middle::mir::{BasicBlock, Body, Local, START_BLOCK};
use rustc_middle::ty::TyKind;

use super::{InsertPoint, InstrKind, MyOptimizationPass, PassLogLevel};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum MetaState {
    Zero,
    Fresh(MetaSymbol),
    Unknown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum MetaSymbol {
    Tag(usize),
    RefAncestor(usize),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct LocalMetaState {
    tag: MetaState,
    ref_ancestor: MetaState,
}

impl LocalMetaState {
    const ZERO: Self = Self {
        tag: MetaState::Zero,
        ref_ancestor: MetaState::Zero,
    };

    const UNKNOWN: Self = Self {
        tag: MetaState::Unknown,
        ref_ancestor: MetaState::Unknown,
    };
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct MetadataLiveness {
    tag_live: HashSet<Local>,
    ref_live: HashSet<Local>,
}

impl MetadataLiveness {
    fn kill_tag(&mut self, local: Local) {
        self.tag_live.remove(&local);
    }

    fn kill_ref(&mut self, local: Local) {
        self.ref_live.remove(&local);
    }

    fn use_tag(&mut self, local: Local) {
        self.tag_live.insert(local);
    }

    fn use_ref(&mut self, local: Local) {
        self.ref_live.insert(local);
    }
}

#[derive(Default)]
struct MetadataDflowStats {
    tag_props_before: usize,
    tag_props_after: usize,
    tag_copies_before: usize,
    tag_copies_after: usize,
    ref_ancestor_copies_before: usize,
    ref_ancestor_copies_after: usize,
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
    for ip in insert_points.iter() {
        if let InstrKind::TagProp {
            copy_tag,
            copy_ref_ancestor,
            ..
        } = ip.kind
        {
            stats.tag_props_before += 1;
            if copy_tag {
                stats.tag_copies_before += 1;
            }
            if copy_ref_ancestor {
                stats.ref_ancestor_copies_before += 1;
            }
        }
    }

    let mut kept = Vec::with_capacity(insert_points.len());
    for (idx, mut ip) in std::mem::take(insert_points).into_iter().enumerate() {
        if let InstrKind::TagProp {
            dst,
            src,
            ref mut copy_tag,
            ref mut copy_ref_ancestor,
        } = ip.kind
        {
            let decision = analysis.tag_prop_decision(idx, dst, src);
            *copy_tag = decision.keep_tag;
            *copy_ref_ancestor = decision.keep_ref_ancestor;
            if !*copy_tag && !*copy_ref_ancestor {
                continue;
            }
        }
        kept.push(ip);
    }
    *insert_points = kept;

    for ip in insert_points.iter() {
        if let InstrKind::TagProp {
            copy_tag,
            copy_ref_ancestor,
            ..
        } = ip.kind
        {
            stats.tag_props_after += 1;
            if copy_tag {
                stats.tag_copies_after += 1;
            }
            if copy_ref_ancestor {
                stats.ref_ancestor_copies_after += 1;
            }
        }
    }

    if metadata_dataflow_stats_enabled() || pass.log_enabled(PassLogLevel::Info) {
        let tag_props_dropped = stats.tag_props_before.saturating_sub(stats.tag_props_after);
        let tag_copies_dropped = stats.tag_copies_before.saturating_sub(stats.tag_copies_after);
        let ref_copies_dropped = stats
            .ref_ancestor_copies_before
            .saturating_sub(stats.ref_ancestor_copies_after);
        if tag_props_dropped != 0 || tag_copies_dropped != 0 || ref_copies_dropped != 0 {
            eprintln!(
                "[rusteze][meta-dflow] tag_props {}->{} dropped={} tag_copies {}->{} dropped={} ref_copies {}->{} dropped={}",
                stats.tag_props_before,
                stats.tag_props_after,
                tag_props_dropped,
                stats.tag_copies_before,
                stats.tag_copies_after,
                tag_copies_dropped,
                stats.ref_ancestor_copies_before,
                stats.ref_ancestor_copies_after,
                ref_copies_dropped,
            );
        }
    }
}

fn metadata_dataflow_enabled() -> bool {
    std::env::var("RZ_METADATA_DATAFLOW")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn metadata_dataflow_stats_enabled() -> bool {
    std::env::var("RZ_METADATA_DATAFLOW_STATS")
        .ok()
        .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

#[derive(Copy, Clone)]
struct TagPropDecision {
    keep_tag: bool,
    keep_ref_ancestor: bool,
}

struct MetadataAnalysis {
    point_states: HashMap<usize, HashMap<Local, LocalMetaState>>,
    live_after: HashMap<usize, MetadataLiveness>,
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
        let successors = block_successors(body);

        let zero_state = pointer_locals
            .iter()
            .copied()
            .map(|local| (local, LocalMetaState::ZERO))
            .collect::<HashMap<_, _>>();

        let mut in_states: HashMap<BasicBlock, HashMap<Local, LocalMetaState>> = HashMap::new();
        let mut out_states: HashMap<BasicBlock, HashMap<Local, LocalMetaState>> = HashMap::new();
        in_states.insert(START_BLOCK, zero_state.clone());

        let mut worklist: Vec<BasicBlock> = body.basic_blocks.indices().collect();
        while let Some(bb) = worklist.pop() {
            let in_state = if bb == START_BLOCK {
                in_states.get(&bb).cloned().unwrap_or_else(|| zero_state.clone())
            } else {
                merge_block_states(&pointer_locals, predecessors.get(&bb), &out_states)
            };

            let out_state = transfer_block(body, &ordered_points, bb, &in_state);
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
                apply_insert_point(point.idx, &point.kind, point.place_local, body, &mut state);
            }
        }

        let mut live_in: HashMap<BasicBlock, MetadataLiveness> = HashMap::new();
        let mut live_out: HashMap<BasicBlock, MetadataLiveness> = HashMap::new();
        let mut live_worklist: Vec<BasicBlock> = body.basic_blocks.indices().collect();
        while let Some(bb) = live_worklist.pop() {
            let out_live = merge_block_liveness(successors.get(&bb), &live_in);
            let in_live = transfer_block_liveness(&ordered_points, bb, &out_live);
            let changed_out = live_out.get(&bb) != Some(&out_live);
            let changed_in = live_in.get(&bb) != Some(&in_live);
            if changed_out {
                live_out.insert(bb, out_live);
            }
            if changed_in {
                live_in.insert(bb, in_live);
                if let Some(preds) = predecessors.get(&bb) {
                    live_worklist.extend(preds.iter().copied());
                }
            }
        }

        let mut live_after = HashMap::new();
        for (bb, points) in ordered_points.iter() {
            let mut live = live_out.get(bb).cloned().unwrap_or_default();
            for point in points.iter().rev() {
                live_after.insert(point.idx, live.clone());
                apply_liveness_for_point(&point.kind, point.place_local, &mut live);
            }
        }

        Self {
            point_states,
            live_after,
        }
    }

    fn tag_prop_decision(&self, idx: usize, dst: Local, src: Local) -> TagPropDecision {
        if dst == src {
            return TagPropDecision {
                keep_tag: false,
                keep_ref_ancestor: false,
            };
        }
        let state = self.point_states.get(&idx);
        let tag_redundant = state
            .and_then(|s| Some(s.get(&dst)?.tag == s.get(&src)?.tag))
            .unwrap_or(false);
        let ref_redundant = state
            .and_then(|s| Some(s.get(&dst)?.ref_ancestor == s.get(&src)?.ref_ancestor))
            .unwrap_or(false);
        TagPropDecision {
            keep_tag: !tag_redundant,
            keep_ref_ancestor: !ref_redundant,
        }
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

fn block_successors<'tcx>(body: &Body<'tcx>) -> HashMap<BasicBlock, Vec<BasicBlock>> {
    let mut successors: HashMap<BasicBlock, Vec<BasicBlock>> = HashMap::new();
    for (bb, data) in body.basic_blocks.iter_enumerated() {
        if let Some(term) = &data.terminator {
            successors.insert(bb, term.successors().collect());
        }
    }
    successors
}

fn merge_block_states(
    pointer_locals: &[Local],
    preds: Option<&Vec<BasicBlock>>,
    out_states: &HashMap<BasicBlock, HashMap<Local, LocalMetaState>>,
) -> HashMap<Local, LocalMetaState> {
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
            .unwrap_or(LocalMetaState::UNKNOWN);
        let same = preds.iter().skip(1).all(|pred| {
            out_states
                .get(pred)
                .and_then(|state| state.get(&local).copied())
                .unwrap_or(LocalMetaState::UNKNOWN)
                == first
        });
        merged.insert(local, if same { first } else { LocalMetaState::UNKNOWN });
    }
    merged
}

fn merge_block_liveness(
    succs: Option<&Vec<BasicBlock>>,
    live_in: &HashMap<BasicBlock, MetadataLiveness>,
) -> MetadataLiveness {
    let Some(succs) = succs else {
        return MetadataLiveness::default();
    };
    let mut merged = MetadataLiveness::default();
    for succ in succs {
        if let Some(succ_live) = live_in.get(succ) {
            merged.tag_live.extend(succ_live.tag_live.iter().copied());
            merged
                .ref_live
                .extend(succ_live.ref_live.iter().copied());
        }
    }
    merged
}

fn transfer_block<'tcx>(
    body: &Body<'tcx>,
    ordered_points: &HashMap<BasicBlock, Vec<OrderedPoint<'tcx>>>,
    bb: BasicBlock,
    in_state: &HashMap<Local, LocalMetaState>,
) -> HashMap<Local, LocalMetaState> {
    let mut state = in_state.clone();
    if let Some(points) = ordered_points.get(&bb) {
        for point in points {
            apply_insert_point(point.idx, &point.kind, point.place_local, body, &mut state);
        }
    }
    state
}

fn transfer_block_liveness<'tcx>(
    ordered_points: &HashMap<BasicBlock, Vec<OrderedPoint<'tcx>>>,
    bb: BasicBlock,
    out_live: &MetadataLiveness,
) -> MetadataLiveness {
    let mut live = out_live.clone();
    if let Some(points) = ordered_points.get(&bb) {
        for point in points.iter().rev() {
            apply_liveness_for_point(&point.kind, point.place_local, &mut live);
        }
    }
    live
}

fn apply_insert_point(
    idx: usize,
    kind: &InstrKind<'_>,
    place_local: Option<Local>,
    body: &Body<'_>,
    state: &mut HashMap<Local, LocalMetaState>,
) {
    match *kind {
        InstrKind::TagProp { dst, src, .. } => {
            let src_state = state.get(&src).copied().unwrap_or(LocalMetaState::UNKNOWN);
            state.insert(dst, src_state);
        }
        InstrKind::Ref { .. } => {
            if let Some(dst) = place_local {
                let tag = MetaState::Fresh(MetaSymbol::Tag(idx));
                state.insert(
                    dst,
                    LocalMetaState {
                        tag,
                        ref_ancestor: tag,
                    },
                );
            }
        }
        InstrKind::Raw { src, .. } => {
            if let Some(dst) = place_local {
                let src_ref = state
                    .get(&src.local)
                    .copied()
                    .unwrap_or(LocalMetaState::UNKNOWN)
                    .ref_ancestor;
                state.insert(
                    dst,
                    LocalMetaState {
                        tag: MetaState::Fresh(MetaSymbol::Tag(idx)),
                        ref_ancestor: src_ref,
                    },
                );
            }
        }
        InstrKind::RawRoot { ptr_local: dst, .. } => {
            state.insert(
                dst,
                LocalMetaState {
                    tag: MetaState::Fresh(MetaSymbol::Tag(idx)),
                    ref_ancestor: MetaState::Zero,
                },
            );
        }
        InstrKind::ArgRetag { ptr_local: dst, .. } => {
            let tag = MetaState::Fresh(MetaSymbol::Tag(idx));
            state.insert(
                dst,
                LocalMetaState {
                    tag,
                    ref_ancestor: match body.local_decls[dst].ty.kind() {
                        TyKind::Ref(..) => tag,
                        TyKind::RawPtr(..) => MetaState::Fresh(MetaSymbol::RefAncestor(idx)),
                        _ => MetaState::Unknown,
                    },
                },
            );
        }
        InstrKind::RetTake { dst_local: dst, .. } => {
            let tag = MetaState::Fresh(MetaSymbol::Tag(idx));
            state.insert(
                dst,
                LocalMetaState {
                    tag,
                    ref_ancestor: tag,
                },
            );
        }
        InstrKind::RetRoot { dst_local: dst, is_ref, .. } => {
            let tag = MetaState::Fresh(MetaSymbol::Tag(idx));
            state.insert(
                dst,
                LocalMetaState {
                    tag,
                    ref_ancestor: if is_ref { tag } else { MetaState::Zero },
                },
            );
        }
        InstrKind::PtrDerive { dst, src, is_ref, .. } => {
            let src_ref = state
                .get(&src)
                .copied()
                .unwrap_or(LocalMetaState::UNKNOWN)
                .ref_ancestor;
            let tag = MetaState::Fresh(MetaSymbol::Tag(idx));
            state.insert(
                dst,
                LocalMetaState {
                    tag,
                    ref_ancestor: if is_ref { tag } else { src_ref },
                },
            );
        }
        _ => {}
    }
}

fn apply_liveness_for_point(
    kind: &InstrKind<'_>,
    place_local: Option<Local>,
    live: &mut MetadataLiveness,
) {
    match kind {
        InstrKind::TagProp {
            dst,
            src,
            copy_tag,
            copy_ref_ancestor,
        } => {
            if *copy_tag {
                live.kill_tag(*dst);
                live.use_tag(*src);
            }
            if *copy_ref_ancestor {
                live.kill_ref(*dst);
                live.use_ref(*src);
            }
        }
        InstrKind::PtrRead { ptr_local, .. }
        | InstrKind::PtrWrite { ptr_local, .. }
        | InstrKind::PtrReadAllowUntagged { ptr_local, .. }
        | InstrKind::PtrWriteAllowUntagged { ptr_local, .. }
        | InstrKind::PtrUse { ptr_local }
        | InstrKind::CallArgPush { ptr_local, .. }
        | InstrKind::RetPush { ptr_local, .. } => {
            live.use_tag(*ptr_local);
        }
        InstrKind::Ref { src, .. } | InstrKind::Raw { src, .. } => {
            if let Some(dst) = place_local {
                live.kill_tag(dst);
                live.kill_ref(dst);
            }
            live.use_tag(src.local);
            live.use_ref(src.local);
        }
        InstrKind::RawRoot { ptr_local, .. }
        | InstrKind::ArgRetag { ptr_local, .. } => {
            live.kill_tag(*ptr_local);
            live.kill_ref(*ptr_local);
        }
        InstrKind::RetTake { dst_local, .. } | InstrKind::RetRoot { dst_local, .. } => {
            live.kill_tag(*dst_local);
            live.kill_ref(*dst_local);
        }
        InstrKind::PtrDerive { dst, src, .. } => {
            live.kill_tag(*dst);
            live.kill_ref(*dst);
            live.use_tag(*src);
            live.use_ref(*src);
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
        // Metadata propagation must be ordered after tag-creating hooks but before
        // access/usage hooks at the same program point. Otherwise liveness can
        // incorrectly conclude that the copied tag is dead and delete the TagProp.
        InstrKind::TagProp { .. } => 1,
        InstrKind::PtrRead { .. }
        | InstrKind::PtrWrite { .. }
        | InstrKind::PtrReadAllowUntagged { .. }
        | InstrKind::PtrWriteAllowUntagged { .. } => 2,
        InstrKind::CallArgPush { .. } | InstrKind::PtrUse { .. } => 3,
        _ => 4,
    }
}
