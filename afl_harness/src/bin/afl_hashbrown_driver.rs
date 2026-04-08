use hashbrown::{
    hash_map::{Entry, RawEntryMut},
    HashMap,
};
use std::hash::{BuildHasher, Hash, Hasher};
use std::hint::black_box;

const SLOT_COUNT: usize = 2;
const MAX_STEPS: usize = 96;
const MAX_ENTRIES: usize = 64;
const MAX_KEY_LEN: usize = 24;
const MAX_VALUE_LEN: usize = 64;

type Map = HashMap<Vec<u8>, Vec<u8>>;

macro_rules! next_byte {
    ($data:expr, $idx:expr) => {{
        if $idx >= $data.len() {
            0
        } else {
            let out = $data[$idx];
            $idx += 1;
            out
        }
    }};
}

macro_rules! bounded_usize {
    ($data:expr, $idx:expr, $bound:expr) => {{
        let bound = $bound;
        if bound == 0 {
            0
        } else {
            (next_byte!($data, $idx) as usize) % bound
        }
    }};
}

macro_rules! size_hint {
    ($data:expr, $idx:expr) => {{
        match next_byte!($data, $idx) % 12 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 7,
            5 => 8,
            6 => 15,
            7 => 16,
            8 => 23,
            9 => 24,
            10 => 31,
            _ => next_byte!($data, $idx) as usize,
        }
    }};
}

macro_rules! take_blob {
    ($data:expr, $idx:expr, $max_len:expr) => {{
        let len = bounded_usize!($data, $idx, $max_len + 1);
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(next_byte!($data, $idx));
        }
        out
    }};
}

macro_rules! take_key {
    ($data:expr, $idx:expr) => {
        take_blob!($data, $idx, MAX_KEY_LEN)
    };
}

macro_rules! take_value {
    ($data:expr, $idx:expr) => {
        take_blob!($data, $idx, MAX_VALUE_LEN)
    };
}

fn compute_hash<K: Hash + ?Sized, S: BuildHasher>(hash_builder: &S, key: &K) -> u64 {
    let mut state = hash_builder.build_hasher();
    key.hash(&mut state);
    state.finish()
}

fn pick_key(map: &Map, cursor_idx: &mut usize, data: &[u8]) -> Vec<u8> {
    if !map.is_empty() && (next_byte!(data, *cursor_idx) & 1) == 0 {
        let key_idx = bounded_usize!(data, *cursor_idx, map.len());
        map.keys().nth(key_idx).cloned().unwrap_or_default()
    } else {
        take_key!(data, *cursor_idx)
    }
}

fn distinct_lookup_keys(
    map: &Map,
    cursor_idx: &mut usize,
    data: &[u8],
) -> Option<(Vec<u8>, Vec<u8>)> {
    if map.len() >= 2 {
        let first = bounded_usize!(data, *cursor_idx, map.len());
        let mut second = bounded_usize!(data, *cursor_idx, map.len());
        if second == first {
            second = (second + 1) % map.len();
        }
        let key_a = map.keys().nth(first)?.clone();
        let key_b = map.keys().nth(second)?.clone();
        if key_a != key_b {
            return Some((key_a, key_b));
        }
    }

    let key_a = pick_key(map, cursor_idx, data);
    let key_b = pick_key(map, cursor_idx, data);
    if key_a == key_b {
        None
    } else {
        Some((key_a, key_b))
    }
}

fn two_maps_mut(maps: &mut [Map; SLOT_COUNT], a: usize, b: usize) -> (&mut Map, &mut Map) {
    debug_assert_ne!(a, b);
    if a < b {
        let (left, right) = maps.split_at_mut(b);
        (&mut left[a], &mut right[0])
    } else {
        let (left, right) = maps.split_at_mut(a);
        (&mut right[0], &mut left[b])
    }
}

fn mutate_value(value: &mut Vec<u8>, cursor_idx: &mut usize, data: &[u8]) {
    match next_byte!(data, *cursor_idx) % 5 {
        0 => {
            if value.len() < MAX_VALUE_LEN {
                value.push(next_byte!(data, *cursor_idx));
            }
        }
        1 => {
            let new_len = bounded_usize!(data, *cursor_idx, value.len() + 1);
            value.truncate(new_len);
        }
        2 => {
            if !value.is_empty() {
                let idx = bounded_usize!(data, *cursor_idx, value.len());
                value[idx] ^= next_byte!(data, *cursor_idx);
            }
        }
        3 => {
            let extra = take_blob!(data, *cursor_idx, 8);
            let room = MAX_VALUE_LEN.saturating_sub(value.len());
            value.extend_from_slice(&extra[..extra.len().min(room)]);
        }
        _ => value.reverse(),
    }
}

fn mutate_owned_value(value: &mut Vec<u8>, cursor_idx: &mut usize, data: &[u8]) {
    mutate_value(value, cursor_idx, data);
    if (next_byte!(data, *cursor_idx) & 1) == 0 && value.len() < MAX_VALUE_LEN {
        value.push(next_byte!(data, *cursor_idx));
    }
}

fn touch_map(map: &Map) {
    black_box(map.len());
    black_box(map.capacity());
}

fn run_step(maps: &mut [Map; SLOT_COUNT], cursor_idx: usize, data: &[u8]) -> usize {
    let mut cursor_idx = cursor_idx;
    let op = next_byte!(data, cursor_idx) % 20;
    let slot = bounded_usize!(data, cursor_idx, SLOT_COUNT);
    let other = 1 - slot;

    match op {
        0 => {
            let capacity = size_hint!(data, cursor_idx).min(MAX_ENTRIES);
            maps[slot].clear();
            maps[slot].shrink_to(0);
            maps[slot].reserve(capacity);
        }
        1 => {
            let key = take_key!(data, cursor_idx);
            let value = take_value!(data, cursor_idx);
            if maps[slot].len() < MAX_ENTRIES || maps[slot].contains_key(key.as_slice()) {
                black_box(maps[slot].insert(key, value));
            }
        }
        2 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            if (next_byte!(data, cursor_idx) & 1) == 0 {
                black_box(maps[slot].remove(key.as_slice()));
            } else {
                black_box(maps[slot].remove_entry(key.as_slice()));
            }
        }
        3 => {
            let additional = size_hint!(data, cursor_idx).min(MAX_ENTRIES);
            if (next_byte!(data, cursor_idx) & 1) == 0 {
                maps[slot].reserve(additional);
            } else {
                let _ = maps[slot].try_reserve(additional);
            }
        }
        4 => {
            if (next_byte!(data, cursor_idx) & 1) == 0 {
                maps[slot].shrink_to_fit();
            } else {
                maps[slot].shrink_to(size_hint!(data, cursor_idx).min(MAX_ENTRIES));
            }
        }
        5 => maps[slot].clear(),
        6 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            let default = take_value!(data, cursor_idx);
            let can_insert = maps[slot].len() < MAX_ENTRIES;
            match next_byte!(data, cursor_idx) % 4 {
                0 => {
                    let value = maps[slot].entry(key).or_insert(default);
                    mutate_value(value, &mut cursor_idx, data);
                }
                1 => {
                    let value = maps[slot]
                        .entry(key)
                        .and_modify(|value| mutate_value(value, &mut cursor_idx, data))
                        .or_insert(default);
                    mutate_value(value, &mut cursor_idx, data);
                }
                2 => {
                    let entry = maps[slot]
                        .entry(key)
                        .and_replace_entry_with(|_, mut value| {
                            mutate_owned_value(&mut value, &mut cursor_idx, data);
                            if (next_byte!(data, cursor_idx) & 1) == 0 {
                                Some(value)
                            } else {
                                None
                            }
                        });
                    match entry {
                        Entry::Occupied(mut occupied) => {
                            mutate_value(occupied.get_mut(), &mut cursor_idx, data)
                        }
                        Entry::Vacant(vacant) => {
                            if can_insert {
                                mutate_value(vacant.insert(default), &mut cursor_idx, data);
                            }
                        }
                    }
                }
                _ => {
                    let mut occupied = maps[slot].entry(key).or_insert_entry(default);
                    mutate_value(occupied.get_mut(), &mut cursor_idx, data);
                    if (next_byte!(data, cursor_idx) & 1) == 0 {
                        black_box(occupied.remove_entry());
                    }
                }
            }
        }
        7 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            let query = key.as_slice();
            let default = take_value!(data, cursor_idx);
            let value = maps[slot]
                .entry_ref(query)
                .and_modify(|value| mutate_value(value, &mut cursor_idx, data))
                .or_insert(default);
            mutate_value(value, &mut cursor_idx, data);
        }
        8 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            match next_byte!(data, cursor_idx) % 3 {
                0 => {
                    if let Some(value) = maps[slot].get(key.as_slice()) {
                        black_box(value.len());
                    }
                }
                1 => {
                    if let Some(value) = maps[slot].get_mut(key.as_slice()) {
                        mutate_value(value, &mut cursor_idx, data);
                    }
                }
                _ => {
                    if let Some((found_key, found_value)) = maps[slot].get_key_value(key.as_slice())
                    {
                        black_box(found_key.len());
                        black_box(found_value.len());
                    }
                }
            }
        }
        9 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            let hash = compute_hash(maps[slot].hasher(), key.as_slice());
            match next_byte!(data, cursor_idx) % 3 {
                0 => {
                    if let Some((found_key, found_value)) =
                        maps[slot].raw_entry().from_key(key.as_slice())
                    {
                        black_box(found_key.len());
                        black_box(found_value.len());
                    }
                }
                1 => {
                    if let Some((found_key, found_value)) = maps[slot]
                        .raw_entry()
                        .from_hash(hash, |candidate| candidate.as_slice() == key.as_slice())
                    {
                        black_box(found_key.len());
                        black_box(found_value.len());
                    }
                }
                _ => {
                    if let Some((found_key, found_value)) = maps[slot]
                        .raw_entry()
                        .from_key_hashed_nocheck(hash, key.as_slice())
                    {
                        black_box(found_key.len());
                        black_box(found_value.len());
                    }
                }
            }
        }
        10 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            let value = take_value!(data, cursor_idx);
            let can_insert = maps[slot].len() < MAX_ENTRIES;
            match maps[slot].raw_entry_mut().from_key(key.as_slice()) {
                RawEntryMut::Occupied(mut occupied) => match next_byte!(data, cursor_idx) % 4 {
                    0 => mutate_value(occupied.get_mut(), &mut cursor_idx, data),
                    1 => {
                        black_box(occupied.insert(value));
                    }
                    2 => {
                        black_box(occupied.remove_entry());
                    }
                    _ => {
                        black_box(
                            match occupied.replace_entry_with(|_, mut old| {
                                mutate_owned_value(&mut old, &mut cursor_idx, data);
                                if (next_byte!(data, cursor_idx) & 1) == 0 {
                                    Some(old)
                                } else {
                                    None
                                }
                            }) {
                                RawEntryMut::Occupied(_) => 1u8,
                                RawEntryMut::Vacant(_) => 0u8,
                            },
                        );
                    }
                },
                RawEntryMut::Vacant(vacant) => {
                    if can_insert {
                        let (_, inserted) = vacant.insert(key, value);
                        mutate_value(inserted, &mut cursor_idx, data);
                    }
                }
            }
        }
        11 => {
            let key = pick_key(&maps[slot], &mut cursor_idx, data);
            let hash = compute_hash(maps[slot].hasher(), key.as_slice());
            let value = take_value!(data, cursor_idx);
            let can_insert =
                maps[slot].len() < MAX_ENTRIES || maps[slot].contains_key(key.as_slice());
            if can_insert {
                let (found_key, found_value) = if (next_byte!(data, cursor_idx) & 1) == 0 {
                    maps[slot]
                        .raw_entry_mut()
                        .from_hash(hash, |candidate| candidate.as_slice() == key.as_slice())
                        .or_insert(key, value)
                } else {
                    maps[slot]
                        .raw_entry_mut()
                        .from_key_hashed_nocheck(hash, key.as_slice())
                        .or_insert(key, value)
                };
                black_box(found_key.len());
                mutate_value(found_value, &mut cursor_idx, data);
            }
        }
        12 => {
            if let Some((key_a, key_b)) = distinct_lookup_keys(&maps[slot], &mut cursor_idx, data) {
                let [left, right] =
                    maps[slot].get_disjoint_mut([key_a.as_slice(), key_b.as_slice()]);
                if let Some(value) = left {
                    mutate_value(value, &mut cursor_idx, data);
                }
                if let Some(value) = right {
                    mutate_value(value, &mut cursor_idx, data);
                }
            }
        }
        13 => {
            let take = bounded_usize!(data, cursor_idx, maps[slot].len() + 1);
            let (src, dst) = two_maps_mut(maps, slot, other);
            let mut drained = src.drain();
            for (key, value) in drained.by_ref().take(take) {
                if dst.len() < MAX_ENTRIES || dst.contains_key(key.as_slice()) {
                    black_box(dst.insert(key, value));
                }
            }
            drop(drained);
        }
        14 => {
            let mask = next_byte!(data, cursor_idx);
            let modulus = bounded_usize!(data, cursor_idx, 5) + 1;
            let limit = bounded_usize!(data, cursor_idx, maps[slot].len() + 1);
            let (src, dst) = two_maps_mut(maps, slot, other);
            let mut extracted = src.extract_if(|key, value| {
                ((key.len() + value.len() + mask as usize) % modulus) == 0
            });
            for (key, value) in extracted.by_ref().take(limit) {
                if dst.len() < MAX_ENTRIES || dst.contains_key(key.as_slice()) {
                    black_box(dst.insert(key, value));
                }
            }
            drop(extracted);
        }
        15 => {
            let tweak = next_byte!(data, cursor_idx);
            let parity = next_byte!(data, cursor_idx) as usize;
            maps[slot].retain(|key, value| {
                if !value.is_empty() {
                    let idx = parity % value.len();
                    value[idx] ^= tweak;
                }
                ((key.len() + value.len() + parity) & 1) == 0
            });
        }
        16 => {
            if (next_byte!(data, cursor_idx) & 1) == 0 {
                let src = maps[other].clone();
                maps[slot].clone_from(&src);
            } else {
                let snapshot = maps[slot].clone();
                let other_snapshot = maps[other].clone();
                maps[slot].clone_from(&other_snapshot);
                maps[other].clone_from(&snapshot);
            }
        }
        17 => {
            let take = bounded_usize!(data, cursor_idx, maps[other].len() + 1);
            let staged: Vec<_> = maps[other]
                .iter()
                .take(take)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            for (key, value) in staged {
                if maps[slot].len() < MAX_ENTRIES || maps[slot].contains_key(key.as_slice()) {
                    black_box(maps[slot].insert(key, value));
                }
            }
        }
        18 => touch_map(&maps[slot]),
        _ => {
            let limit = bounded_usize!(data, cursor_idx, maps[slot].len() + 1);
            for (idx, (key, value)) in maps[slot].iter_mut().enumerate().take(limit) {
                if !value.is_empty() {
                    let pos = idx % value.len();
                    value[pos] ^= next_byte!(data, cursor_idx);
                }
                black_box(key.len());
                black_box(value.len());
            }
        }
    }

    touch_map(&maps[slot]);
    touch_map(&maps[other]);
    cursor_idx
}

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let max_steps = std::env::var("AFL_MAX_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(MAX_STEPS);
    let mut cursor_idx = 0usize;
    let requested_steps = next_byte!(&data, cursor_idx) as usize;
    let steps = requested_steps.min(max_steps).max(1);
    let mut maps = std::array::from_fn(|_| HashMap::new());
    for _ in 0..steps {
        cursor_idx = run_step(&mut maps, cursor_idx, &data);
    }

    touch_map(&maps[0]);
    touch_map(&maps[1]);
    std::mem::forget(maps);
}
