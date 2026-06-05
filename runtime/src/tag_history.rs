use crate::{PtrKind, TagMeta};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TagHistoryState {
    Active,
    HistoricalLive,
    Invalidated,
    DeadCompacted,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct TagHistoryEntry {
    pub tag: u64,
    pub pointee_addr: usize,
    pub kind: PtrKind,
    pub parent: u64,
    pub escaped: bool,
    pub alias_exempt: bool,
    pub state: TagHistoryState,
    pub shadowed_candidate: bool,
}

#[derive(Copy, Clone, Debug)]
struct TagLocation {
    origin_base: usize,
    alloc_epoch: u64,
    state: TagHistoryState,
}

#[derive(Default)]
struct TagHistoryBucket {
    entries: Vec<TagHistoryEntry>,
    latest_for_class: HashMap<(usize, PtrKind), usize>,
    shadowed_candidate_count: usize,
}

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct TagHistoryStats {
    pub active_epoch_buckets: usize,
    pub active_tag_entries: usize,
    pub historical_live_tag_entries: usize,
    pub invalidated_tag_entries: usize,
    pub dead_epoch_buckets: usize,
    pub dead_tag_entries: usize,
    pub shadowed_old_live_tag_candidates: usize,
}

type EpochKey = (usize, u64);

static ACTIVE_HISTORY: OnceLock<Mutex<HashMap<EpochKey, TagHistoryBucket>>> = OnceLock::new();
static DEAD_HISTORY: OnceLock<Mutex<HashMap<EpochKey, TagHistoryBucket>>> = OnceLock::new();
static TAG_LOCATIONS: OnceLock<Mutex<HashMap<u64, TagLocation>>> = OnceLock::new();

#[inline]
fn active_history() -> &'static Mutex<HashMap<EpochKey, TagHistoryBucket>> {
    ACTIVE_HISTORY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn dead_history() -> &'static Mutex<HashMap<EpochKey, TagHistoryBucket>> {
    DEAD_HISTORY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn tag_locations() -> &'static Mutex<HashMap<u64, TagLocation>> {
    TAG_LOCATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
pub(crate) fn remember_live_tag(tag: u64, tmeta: &TagMeta) -> Option<EpochKey> {
    if tag == 0 || !tmeta.origin_known || tmeta.origin_base == 0 || tmeta.alloc_epoch == 0 {
        return None;
    }

    let entry = TagHistoryEntry {
        tag,
        pointee_addr: tmeta.pointee_addr,
        kind: tmeta.kind,
        parent: tmeta.parent,
        escaped: tmeta.escaped,
        alias_exempt: tmeta.alias_exempt,
        state: TagHistoryState::Active,
        shadowed_candidate: false,
    };
    let key = (tmeta.origin_base, tmeta.alloc_epoch);
    let mut reached_candidate_state = false;

    let mut active = active_history().lock().unwrap();
    let bucket = active.entry(key).or_default();
    let class = (entry.pointee_addr, entry.kind);
    if let Some(prev_idx) = bucket.latest_for_class.get(&class).copied() {
        let prev = &mut bucket.entries[prev_idx];
        if !prev.shadowed_candidate
            && prev.state == TagHistoryState::Active
            && prev.parent != 0
            && !prev.escaped
            && !prev.alias_exempt
        {
            prev.shadowed_candidate = true;
            bucket.shadowed_candidate_count += 1;
            reached_candidate_state = true;
        }
    }
    let new_idx = bucket.entries.len();
    bucket.entries.push(entry);
    bucket.latest_for_class.insert(class, new_idx);
    tag_locations().lock().unwrap().insert(
        tag,
        TagLocation {
            origin_base: tmeta.origin_base,
            alloc_epoch: tmeta.alloc_epoch,
            state: TagHistoryState::Active,
        },
    );
    if reached_candidate_state {
        Some(key)
    } else {
        None
    }
}

#[inline]
pub(crate) fn mark_tag_escaped(tag: u64, tmeta: &TagMeta) {
    if tag == 0 {
        return;
    }
    let Some(location) = tag_locations().lock().unwrap().get(&tag).copied() else {
        return;
    };
    let key = (location.origin_base, location.alloc_epoch);
    let history = match location.state {
        TagHistoryState::Active
        | TagHistoryState::HistoricalLive
        | TagHistoryState::Invalidated => active_history(),
        TagHistoryState::DeadCompacted => dead_history(),
    };
    let mut buckets = history.lock().unwrap();
    let Some(bucket) = buckets.get_mut(&key) else {
        return;
    };
    if let Some(entry) = bucket.entries.iter_mut().find(|entry| entry.tag == tag) {
        let was_shadowed_candidate = entry.shadowed_candidate;
        entry.escaped = tmeta.escaped;
        entry.alias_exempt = tmeta.alias_exempt;
        entry.parent = tmeta.parent;
        if was_shadowed_candidate && (entry.escaped || entry.alias_exempt || entry.parent == 0) {
            entry.shadowed_candidate = false;
            bucket.shadowed_candidate_count = bucket.shadowed_candidate_count.saturating_sub(1);
        }
    }
}

#[inline]
pub(crate) fn note_live_tag_pruned(tag: u64) {
    if tag == 0 {
        return;
    }
    let Some(location) = tag_locations().lock().unwrap().get(&tag).copied() else {
        return;
    };
    if location.state != TagHistoryState::Active {
        return;
    }
    let key = (location.origin_base, location.alloc_epoch);
    let mut active = active_history().lock().unwrap();
    let Some(bucket) = active.get_mut(&key) else {
        return;
    };
    if let Some(entry) = bucket.entries.iter_mut().find(|entry| entry.tag == tag) {
        if entry.shadowed_candidate {
            entry.shadowed_candidate = false;
            bucket.shadowed_candidate_count = bucket.shadowed_candidate_count.saturating_sub(1);
        }
        entry.state = TagHistoryState::HistoricalLive;
    }
    if let Some(loc) = tag_locations().lock().unwrap().get_mut(&tag) {
        loc.state = TagHistoryState::HistoricalLive;
    }
}

#[inline]
pub(crate) fn note_invalidated_tag_compacted(tag: u64) {
    if tag == 0 {
        return;
    }
    let Some(location) = tag_locations().lock().unwrap().get(&tag).copied() else {
        return;
    };
    if matches!(location.state, TagHistoryState::DeadCompacted) {
        return;
    }
    let key = (location.origin_base, location.alloc_epoch);
    let mut active = active_history().lock().unwrap();
    let Some(bucket) = active.get_mut(&key) else {
        return;
    };
    if let Some(entry) = bucket.entries.iter_mut().find(|entry| entry.tag == tag) {
        if entry.shadowed_candidate {
            entry.shadowed_candidate = false;
            bucket.shadowed_candidate_count = bucket.shadowed_candidate_count.saturating_sub(1);
        }
        entry.state = TagHistoryState::Invalidated;
    }
    if let Some(loc) = tag_locations().lock().unwrap().get_mut(&tag) {
        loc.state = TagHistoryState::Invalidated;
    }
}

#[inline]
pub(crate) fn drain_shadowed_candidates(
    base_addr: usize,
    alloc_epoch: u64,
    limit: usize,
) -> Vec<u64> {
    if base_addr == 0 || alloc_epoch == 0 || limit == 0 {
        return Vec::new();
    }

    let key = (base_addr, alloc_epoch);
    let mut active = active_history().lock().unwrap();
    let Some(bucket) = active.get_mut(&key) else {
        return Vec::new();
    };

    let mut drained = Vec::new();
    for entry in &mut bucket.entries {
        if drained.len() >= limit {
            break;
        }
        if entry.state == TagHistoryState::Active && entry.shadowed_candidate {
            entry.shadowed_candidate = false;
            bucket.shadowed_candidate_count = bucket.shadowed_candidate_count.saturating_sub(1);
            drained.push(entry.tag);
        }
    }
    drained
}

#[inline]
pub(crate) fn shadowed_candidate_count(base_addr: usize, alloc_epoch: u64) -> usize {
    if base_addr == 0 || alloc_epoch == 0 {
        return 0;
    }
    let key = (base_addr, alloc_epoch);
    active_history()
        .lock()
        .unwrap()
        .get(&key)
        .map_or(0, |bucket| bucket.shadowed_candidate_count)
}

#[inline]
pub(crate) fn note_dead_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    let key = (base_addr, alloc_epoch);
    let Some(mut bucket) = active_history().lock().unwrap().remove(&key) else {
        return;
    };
    for entry in &mut bucket.entries {
        entry.state = TagHistoryState::DeadCompacted;
        entry.shadowed_candidate = false;
    }
    {
        let mut locations = tag_locations().lock().unwrap();
        for entry in &bucket.entries {
            if let Some(loc) = locations.get_mut(&entry.tag) {
                loc.state = TagHistoryState::DeadCompacted;
            }
        }
    }
    bucket.shadowed_candidate_count = 0;
    dead_history().lock().unwrap().insert(key, bucket);
}

#[inline]
pub(crate) fn stats() -> TagHistoryStats {
    let active = active_history().lock().unwrap();
    let dead = dead_history().lock().unwrap();
    TagHistoryStats {
        active_epoch_buckets: active.len(),
        active_tag_entries: active
            .values()
            .map(|bucket| {
                bucket
                    .entries
                    .iter()
                    .filter(|entry| entry.state == TagHistoryState::Active)
                    .count()
            })
            .sum(),
        historical_live_tag_entries: active
            .values()
            .map(|bucket| {
                bucket
                    .entries
                    .iter()
                    .filter(|entry| entry.state == TagHistoryState::HistoricalLive)
                    .count()
            })
            .sum(),
        invalidated_tag_entries: active
            .values()
            .map(|bucket| {
                bucket
                    .entries
                    .iter()
                    .filter(|entry| entry.state == TagHistoryState::Invalidated)
                    .count()
            })
            .sum(),
        dead_epoch_buckets: dead.len(),
        dead_tag_entries: dead.values().map(|bucket| bucket.entries.len()).sum(),
        shadowed_old_live_tag_candidates: active
            .values()
            .map(|bucket| bucket.shadowed_candidate_count)
            .sum(),
    }
}
