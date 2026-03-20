use crate::{PtrKind, TagMeta};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TagHistoryState {
    Active,
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
}

#[derive(Copy, Clone, Debug)]
struct TagLocation {
    origin_base: usize,
    alloc_epoch: u64,
    state: TagHistoryState,
}

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct TagHistoryStats {
    pub active_epoch_buckets: usize,
    pub active_tag_entries: usize,
    pub dead_epoch_buckets: usize,
    pub dead_tag_entries: usize,
    pub shadowed_old_live_tag_candidates: usize,
}

type EpochKey = (usize, u64);

static ACTIVE_HISTORY: OnceLock<Mutex<HashMap<EpochKey, Vec<TagHistoryEntry>>>> = OnceLock::new();
static DEAD_HISTORY: OnceLock<Mutex<HashMap<EpochKey, Vec<TagHistoryEntry>>>> = OnceLock::new();
static TAG_LOCATIONS: OnceLock<Mutex<HashMap<u64, TagLocation>>> = OnceLock::new();

#[inline]
fn active_history() -> &'static Mutex<HashMap<EpochKey, Vec<TagHistoryEntry>>> {
    ACTIVE_HISTORY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn dead_history() -> &'static Mutex<HashMap<EpochKey, Vec<TagHistoryEntry>>> {
    DEAD_HISTORY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn tag_locations() -> &'static Mutex<HashMap<u64, TagLocation>> {
    TAG_LOCATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
pub(crate) fn remember_live_tag(tag: u64, tmeta: &TagMeta) {
    if tag == 0 || !tmeta.origin_known || tmeta.origin_base == 0 || tmeta.alloc_epoch == 0 {
        return;
    }

    let entry = TagHistoryEntry {
        tag,
        pointee_addr: tmeta.pointee_addr,
        kind: tmeta.kind,
        parent: tmeta.parent,
        escaped: tmeta.escaped,
        alias_exempt: tmeta.alias_exempt,
        state: TagHistoryState::Active,
    };
    let key = (tmeta.origin_base, tmeta.alloc_epoch);

    active_history()
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .push(entry);
    tag_locations().lock().unwrap().insert(
        tag,
        TagLocation {
            origin_base: tmeta.origin_base,
            alloc_epoch: tmeta.alloc_epoch,
            state: TagHistoryState::Active,
        },
    );
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
        TagHistoryState::Active => active_history(),
        TagHistoryState::DeadCompacted => dead_history(),
    };
    let mut buckets = history.lock().unwrap();
    let Some(entries) = buckets.get_mut(&key) else {
        return;
    };
    if let Some(entry) = entries.iter_mut().find(|entry| entry.tag == tag) {
        entry.escaped = tmeta.escaped;
        entry.alias_exempt = tmeta.alias_exempt;
        entry.parent = tmeta.parent;
    }
}

#[inline]
pub(crate) fn note_dead_epoch(base_addr: usize, alloc_epoch: u64) {
    if base_addr == 0 || alloc_epoch == 0 {
        return;
    }
    let key = (base_addr, alloc_epoch);
    let Some(mut entries) = active_history().lock().unwrap().remove(&key) else {
        return;
    };
    for entry in &mut entries {
        entry.state = TagHistoryState::DeadCompacted;
    }
    {
        let mut locations = tag_locations().lock().unwrap();
        for entry in &entries {
            if let Some(loc) = locations.get_mut(&entry.tag) {
                loc.state = TagHistoryState::DeadCompacted;
            }
        }
    }
    dead_history().lock().unwrap().insert(key, entries);
}

#[inline]
fn shadowed_old_live_tag_candidates_in(entries: &[TagHistoryEntry]) -> usize {
    let mut newest_for_class: HashMap<(usize, PtrKind), usize> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        newest_for_class.insert((entry.pointee_addr, entry.kind), idx);
    }

    entries
        .iter()
        .enumerate()
        .filter(|(idx, entry)| {
            entry.state == TagHistoryState::Active
                && entry.parent != 0
                && !entry.escaped
                && !entry.alias_exempt
                && newest_for_class
                    .get(&(entry.pointee_addr, entry.kind))
                    .copied()
                    .is_some_and(|newest_idx| newest_idx > *idx)
        })
        .count()
}

#[inline]
pub(crate) fn stats() -> TagHistoryStats {
    let active = active_history().lock().unwrap();
    let dead = dead_history().lock().unwrap();
    TagHistoryStats {
        active_epoch_buckets: active.len(),
        active_tag_entries: active.values().map(Vec::len).sum(),
        dead_epoch_buckets: dead.len(),
        dead_tag_entries: dead.values().map(Vec::len).sum(),
        shadowed_old_live_tag_candidates: active
            .values()
            .map(|entries| shadowed_old_live_tag_candidates_in(entries))
            .sum(),
    }
}
