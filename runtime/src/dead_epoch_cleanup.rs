use crate::{ptr_shadow, tag_pruning, tag_store};

/// Reclaim live-only auxiliary state for a dead allocation epoch.
///
/// This is intentionally conservative:
/// - active tags are compacted into the dead-tag store
/// - pointer shadow and pruning side state for the dead epoch is removed
///
/// We do not delete dead-tag history here because stale-pointer detection still
/// needs that metadata after free / stack-pop.
#[inline]
pub(crate) fn reclaim_alloc_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    tag_store::compact_alloc_epoch(base_addr, alloc_epoch);
    ptr_shadow::remove_alloc_epoch(base_addr, alloc_epoch);
    tag_pruning::note_dead_epoch(base_addr, alloc_epoch);
}
