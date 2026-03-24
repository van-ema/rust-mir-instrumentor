use crate::{alias_model::active_alias_model, tag_history, tag_store, TagMeta};
use std::sync::OnceLock;

pub(crate) use tag_history::TagHistoryStats;

#[inline]
fn rz_prune_old_live_tags_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RZ_PRUNE_OLD_LIVE_TAGS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    })
}

#[inline]
fn can_prune_live_tags() -> bool {
    rz_prune_old_live_tags_enabled() && active_alias_model().name() == "none"
}

#[inline]
fn rz_prune_old_live_tags_batch_size() -> usize {
    static BATCH_SIZE: OnceLock<usize> = OnceLock::new();
    *BATCH_SIZE.get_or_init(|| {
        std::env::var("RZ_PRUNE_OLD_LIVE_TAGS_BATCH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n != 0)
            .unwrap_or(64)
    })
}

#[inline]
fn maybe_prune_shadowed_old_live_tags(base_addr: usize, alloc_epoch: u64) {
    if !can_prune_live_tags() || base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    let batch_size = rz_prune_old_live_tags_batch_size();
    let pending = tag_history::shadowed_candidate_count(base_addr, alloc_epoch);
    if pending < batch_size {
        return;
    }
    for tag in tag_history::drain_shadowed_candidates(base_addr, alloc_epoch, batch_size) {
        if tag_store::compact_live_tag(tag) {
            tag_history::note_live_tag_pruned(tag);
        }
    }
}

/// Conservative old-live-tag pruning.
///
/// The first enabled rule is deliberately narrow: when new tags shadow older
/// tags with the same `(pointee_addr, PtrKind)` in the same live allocation
/// epoch, and the older tags are still unescaped, non-root, and
/// alias-exempt-free, we compact them out of the active tag store in deferred
/// batches rather than on every shadow event.
///
/// This path is opt-in and currently restricted to `RZ_ALIAS_MODEL=none`, where
/// accesses consume only spatial/temporal metadata and do not depend on
/// alias-model-internal per-tag state.
#[inline]
pub(crate) fn remember_live_tag(tag: u64, tmeta: &TagMeta) {
    if let Some((base_addr, alloc_epoch)) = tag_history::remember_live_tag(tag, tmeta) {
        maybe_prune_shadowed_old_live_tags(base_addr, alloc_epoch);
    }
}

#[inline]
pub(crate) fn mark_tag_escaped(tag: u64, tmeta: &TagMeta) {
    tag_history::mark_tag_escaped(tag, tmeta);
}

#[inline]
pub(crate) fn note_dead_epoch(base_addr: usize, alloc_epoch: u64) {
    tag_history::note_dead_epoch(base_addr, alloc_epoch);
}

#[inline]
pub(crate) fn stats() -> TagHistoryStats {
    tag_history::stats()
}
