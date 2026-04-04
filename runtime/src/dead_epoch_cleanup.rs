use crate::{exact_parent_index, lineage_cache, ptr_shadow, tag_pruning, tag_store};

/// Reclaim live-only auxiliary state for a dead allocation epoch.
///
/// This is intentionally conservative:
/// - active tags are compacted into the dead-tag store
/// - exact-parent recovery entries for the dead epoch are removed
/// - lineage cache is only notified so stale thread-local entries self-invalidate
///
/// We do not delete dead-tag history here because stale-pointer detection still
/// needs that metadata after free / stack-pop.
#[inline]
pub(crate) fn reclaim_alloc_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    tag_store::compact_alloc_epoch(base_addr, alloc_epoch);
    exact_parent_index::remove_alloc_epoch(base_addr, alloc_epoch);
    lineage_cache::note_dead_epoch(base_addr, alloc_epoch);
    ptr_shadow::remove_alloc_epoch(base_addr, alloc_epoch);
    tag_pruning::note_dead_epoch(base_addr, alloc_epoch);
}
