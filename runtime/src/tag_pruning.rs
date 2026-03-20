use crate::{tag_history, TagMeta};

pub(crate) use tag_history::TagHistoryStats;

/// Diagnostic-only scaffolding for future old-tag pruning.
///
/// This module does not delete or rewrite tags. It only tracks per-allocation
/// tag history and reports conservative candidate counts for older live tags
/// that appear shadowed by newer tags in the same allocation epoch.
#[inline]
pub(crate) fn remember_live_tag(tag: u64, tmeta: &TagMeta) {
    tag_history::remember_live_tag(tag, tmeta);
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
