use indexmap::IndexMap;
use std::{collections::hash_map::DefaultHasher, env, hash::BuildHasherDefault, hint::black_box};

const SLOT_COUNT: usize = 2;
const POOL_SLOTS: usize = 2;
const MAX_STEPS: usize = 96;
const MAX_MAP_LEN: usize = 64;
const MAX_KEY_LEN: usize = 24;
const MAX_VALUE_LEN: usize = 48;

type FuzzMap = IndexMap<Vec<u8>, Vec<u8>, BuildHasherDefault<DefaultHasher>>;

struct Cursor<'a> {
    data: &'a [u8],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, idx: 0 }
    }

    fn is_empty(&self) -> bool {
        self.idx >= self.data.len()
    }

    fn byte(&mut self) -> u8 {
        if self.idx >= self.data.len() {
            0
        } else {
            let out = self.data[self.idx];
            self.idx += 1;
            out
        }
    }

    fn bounded_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.byte() as usize) % bound
        }
    }

    fn take_vec(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.bounded_usize(max_len + 1);
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.byte());
        }
        out
    }
}

fn select_ref<'a, T>(first: &'a T, second: &'a T, idx: usize) -> &'a T {
    if idx == 0 {
        first
    } else {
        second
    }
}

fn select_mut<'a, T>(first: &'a mut T, second: &'a mut T, idx: usize) -> &'a mut T {
    if idx == 0 {
        first
    } else {
        second
    }
}

fn select_pair_mut<'a, T>(
    first: &'a mut T,
    second: &'a mut T,
    idx: usize,
) -> (&'a mut T, &'a mut T) {
    if idx == 0 {
        (first, second)
    } else {
        (second, first)
    }
}

fn mutate_blob(blob: &mut Vec<u8>, cursor: &mut Cursor<'_>, max_len: usize) {
    match cursor.byte() % 6 {
        0 => {
            *blob = cursor.take_vec(max_len);
        }
        1 => {
            let extra = cursor.bounded_usize(max_len.saturating_sub(blob.len()).min(8) + 1);
            for _ in 0..extra {
                blob.push(cursor.byte());
            }
        }
        2 => {
            if !blob.is_empty() {
                let new_len = cursor.bounded_usize(blob.len() + 1);
                blob.truncate(new_len);
            }
        }
        3 => {
            if !blob.is_empty() {
                let idx = cursor.bounded_usize(blob.len());
                blob[idx] ^= cursor.byte();
            }
        }
        4 => blob.reverse(),
        _ => {
            if blob.len() >= 2 {
                let a = cursor.bounded_usize(blob.len());
                let mut b = cursor.bounded_usize(blob.len());
                if a == b {
                    b = (b + 1) % blob.len();
                }
                blob.swap(a, b);
            }
        }
    }
}

fn map_range(len: usize, cursor: &mut Cursor<'_>) -> std::ops::Range<usize> {
    let start = cursor.bounded_usize(len + 1);
    let end = start + cursor.bounded_usize(len.saturating_sub(start) + 1);
    start..end
}

fn cap_map(map: &mut FuzzMap) {
    if map.len() > MAX_MAP_LEN {
        map.truncate(MAX_MAP_LEN);
    }
}

fn choose_existing_key(map: &FuzzMap, cursor: &mut Cursor<'_>) -> Option<Vec<u8>> {
    if map.is_empty() {
        None
    } else {
        let idx = cursor.bounded_usize(map.len());
        map.get_index(idx).map(|(key, _)| key.clone())
    }
}

fn choose_key(map: &FuzzMap, pool: &Vec<u8>, cursor: &mut Cursor<'_>) -> Vec<u8> {
    let mode = if existing_key_selection_enabled() {
        3
    } else {
        2
    };
    match cursor.byte() % mode {
        0 => pool.clone(),
        1 => choose_existing_key(map, cursor).unwrap_or_else(|| pool.clone()),
        _ => cursor.take_vec(MAX_KEY_LEN),
    }
}

fn choose_value(pool: &Vec<u8>, cursor: &mut Cursor<'_>) -> Vec<u8> {
    match cursor.byte() % 3 {
        0 => pool.clone(),
        1 => cursor.take_vec(MAX_VALUE_LEN),
        _ => {
            let mut value = pool.clone();
            mutate_blob(&mut value, cursor, MAX_VALUE_LEN);
            value
        }
    }
}

fn mutate_value(value: &mut Vec<u8>, cursor: &mut Cursor<'_>) {
    mutate_blob(value, cursor, MAX_VALUE_LEN);
}

fn score_map(map: &FuzzMap) -> u64 {
    let mut acc = map.len() as u64;
    for (idx, (key, value)) in map.iter().take(8).enumerate() {
        acc ^= ((idx as u64) << 32) ^ (key.len() as u64) ^ ((value.len() as u64) << 16);
        if let Some(&byte) = key.first() {
            acc = acc.rotate_left(7) ^ byte as u64;
        }
        if let Some(&byte) = value.first() {
            acc = acc.rotate_left(11) ^ byte as u64;
        }
    }
    acc
}

fn entry_enabled() -> bool {
    matches!(
        env::var("INDEXMAP_ENABLE_ENTRY").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn existing_key_selection_enabled() -> bool {
    matches!(
        env::var("INDEXMAP_EXISTING_KEYS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn extend_enabled() -> bool {
    matches!(
        env::var("INDEXMAP_ENABLE_EXTEND").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn mutate_with_entry(map: &mut FuzzMap, key: Vec<u8>, value: Vec<u8>, cursor: &mut Cursor<'_>) {
    match map.entry(key) {
        indexmap::map::Entry::Occupied(mut occ) => match cursor.byte() % 3 {
            0 => mutate_value(occ.get_mut(), cursor),
            1 => {
                let mut replacement = value;
                mutate_value(&mut replacement, cursor);
                let _ = occ.insert(replacement);
            }
            _ => {
                let (old_key, old_value) = occ.swap_remove_entry();
                black_box(old_key.len() ^ old_value.len());
            }
        },
        indexmap::map::Entry::Vacant(vac) => {
            let mut inserted = value;
            if (cursor.byte() & 1) == 0 {
                mutate_value(&mut inserted, cursor);
            }
            vac.insert(inserted);
        }
    }
    cap_map(map);
}

fn run_step(
    map0: &mut FuzzMap,
    map1: &mut FuzzMap,
    key_pool0: &mut Vec<u8>,
    key_pool1: &mut Vec<u8>,
    value_pool0: &mut Vec<u8>,
    value_pool1: &mut Vec<u8>,
    cursor: &mut Cursor<'_>,
    sink: &mut u64,
) {
    let slot = cursor.bounded_usize(SLOT_COUNT);
    let pool_a = cursor.bounded_usize(POOL_SLOTS);
    let pool_b = 1 - pool_a;

    match cursor.byte() % 20 {
        0 => mutate_blob(
            select_mut(key_pool0, key_pool1, pool_a),
            cursor,
            MAX_KEY_LEN,
        ),
        1 => mutate_blob(
            select_mut(value_pool0, value_pool1, pool_a),
            cursor,
            MAX_VALUE_LEN,
        ),
        2 => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let value = {
                let pool = select_ref(&*value_pool0, &*value_pool1, pool_b);
                choose_value(pool, cursor)
            };
            let map = select_mut(map0, map1, slot);
            let (idx, old) = map.insert_full(key, value);
            *sink ^= idx as u64;
            *sink ^= old.map_or(0, |v| v.len() as u64);
            cap_map(map);
        }
        3 => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let value = {
                let pool = select_ref(&*value_pool0, &*value_pool1, pool_b);
                choose_value(pool, cursor)
            };
            let map = select_mut(map0, map1, slot);
            if entry_enabled() {
                mutate_with_entry(map, key, value, cursor);
            } else if let Some((idx, found_key, existing)) = map.get_full_mut(&key) {
                *sink ^= idx as u64 ^ found_key.len() as u64;
                mutate_value(existing, cursor);
            } else {
                let (idx, old) = map.insert_full(key, value);
                *sink ^= idx as u64;
                *sink ^= old.map_or(0, |v| v.len() as u64);
                cap_map(map);
            }
        }
        4 => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let map = select_mut(map0, map1, slot);
            if let Some((idx, found_key, value)) = map.get_full_mut(&key) {
                *sink ^= idx as u64 ^ found_key.len() as u64;
                mutate_value(value, cursor);
            }
        }
        5 => {
            let map = select_mut(map0, map1, slot);
            if !map.is_empty() {
                let idx = cursor.bounded_usize(map.len());
                if let Some((key, value)) = map.get_index_mut(idx) {
                    *sink ^= key.len() as u64;
                    mutate_value(value, cursor);
                }
            }
        }
        6 => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let removed = select_mut(map0, map1, slot)
                .swap_remove(&key)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            *sink ^= removed;
        }
        7 => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let removed = select_mut(map0, map1, slot)
                .shift_remove(&key)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            *sink ^= removed;
        }
        8 => {
            let map = select_mut(map0, map1, slot);
            if !map.is_empty() {
                let idx = cursor.bounded_usize(map.len());
                if let Some((key, value)) = map.swap_remove_index(idx) {
                    *sink ^= key.len() as u64 ^ ((value.len() as u64) << 8);
                }
            }
        }
        9 => {
            let map = select_mut(map0, map1, slot);
            if !map.is_empty() {
                let idx = cursor.bounded_usize(map.len());
                if let Some((key, value)) = map.shift_remove_index(idx) {
                    *sink ^= key.len() as u64 ^ ((value.len() as u64) << 12);
                }
            }
        }
        10 => {
            let map = select_mut(map0, map1, slot);
            let len = map.len();
            if len >= 2 {
                let from = cursor.bounded_usize(len);
                let mut to = cursor.bounded_usize(len);
                if from == to {
                    to = (to + 1) % len;
                }
                if (cursor.byte() & 1) == 0 {
                    map.move_index(from, to);
                } else {
                    map.swap_indices(from, to);
                }
            }
        }
        11 => {
            let map = select_mut(map0, map1, slot);
            let range = map_range(map.len(), cursor);
            let drained: usize = map
                .drain(range)
                .map(|(key, value)| key.len() ^ value.len())
                .sum();
            *sink ^= drained as u64;
        }
        12 => {
            let map = select_mut(map0, map1, slot);
            let range = map_range(map.len(), cursor);
            let mask = cursor.byte();
            let removed: usize = map
                .extract_if(range, |key, value| {
                    if !value.is_empty() {
                        let idx = (mask as usize) % value.len();
                        value[idx] ^= key.first().copied().unwrap_or(0);
                    }
                    ((key.len() ^ value.len()) as u8 ^ mask) & 1 == 0
                })
                .map(|(key, value)| key.len() + value.len())
                .sum();
            *sink ^= removed as u64;
        }
        13 => {
            let map = select_mut(map0, map1, slot);
            let mask = cursor.byte();
            map.retain(|key, value| {
                if !value.is_empty() {
                    let idx = (mask as usize) % value.len();
                    value[idx] = value[idx].wrapping_add(mask ^ key.first().copied().unwrap_or(0));
                }
                ((key.len() + value.len()) as u8 ^ mask) % 3 != 0
            });
        }
        14 => {
            let map = select_mut(map0, map1, slot);
            match cursor.byte() % 4 {
                0 => map.reserve(cursor.bounded_usize(16)),
                1 => map.reserve_exact(cursor.bounded_usize(16)),
                2 => map.shrink_to_fit(),
                _ => map.shrink_to(cursor.bounded_usize(map.len() + 1)),
            }
        }
        15 => {
            let map = select_mut(map0, map1, slot);
            if (cursor.byte() & 1) == 0 {
                map.reverse();
            } else {
                map.sort_by(|ka, va, kb, vb| {
                    ka.len()
                        .cmp(&kb.len())
                        .then(va.len().cmp(&vb.len()))
                        .then_with(|| ka.cmp(kb))
                        .then_with(|| va.cmp(vb))
                });
            }
            *sink ^= score_map(map);
        }
        16 => {
            let map = select_mut(map0, map1, slot);
            let at = cursor.bounded_usize(map.len() + 1);
            let tail = map.split_off(at);
            if (cursor.byte() & 1) == 0 {
                if extend_enabled() {
                    map.extend(tail);
                } else {
                    for (key, value) in tail {
                        map.insert_full(key, value);
                    }
                }
                cap_map(map);
            } else {
                *sink ^= score_map(&tail);
            }
        }
        17 => {
            let (dst, src) = select_pair_mut(map0, map1, slot);
            if (cursor.byte() & 1) == 0 {
                dst.clone_from(src);
            } else {
                if extend_enabled() {
                    let additions: Vec<_> = src
                        .iter()
                        .take(8)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    dst.extend(additions);
                } else {
                    for (key, value) in src.iter().take(8) {
                        dst.insert_full(key.clone(), value.clone());
                    }
                }
                cap_map(dst);
            }
        }
        18 => {
            let map = select_mut(map0, map1, slot);
            if (cursor.byte() & 1) == 0 {
                if let Some((key, value)) = map.pop() {
                    *sink ^= key.len() as u64 ^ ((value.len() as u64) << 20);
                }
            } else {
                let mask = cursor.byte();
                if let Some((key, value)) = map.pop_if(|key, value| {
                    if !value.is_empty() {
                        let idx = (mask as usize) % value.len();
                        value[idx] ^= mask;
                    }
                    key.len() % 2 == (mask as usize & 1)
                }) {
                    *sink ^= key.len() as u64 ^ ((value.len() as u64) << 24);
                }
            }
        }
        _ => {
            let key = {
                let map = select_ref(&*map0, &*map1, slot);
                let pool = select_ref(&*key_pool0, &*key_pool1, pool_a);
                choose_key(map, pool, cursor)
            };
            let map = select_ref(&*map0, &*map1, slot);
            *sink ^= map.get_index_of(&key).map_or(0, |idx| idx as u64);
            if !map.is_empty() {
                let idx = cursor.bounded_usize(map.len());
                if let Some((found_key, value)) = map.get_index(idx) {
                    *sink ^= found_key.len() as u64 ^ ((value.len() as u64) << 4);
                }
            }
            *sink ^= score_map(map);
        }
    }
}

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut cursor = Cursor::new(&data);
    let steps = cursor
        .bounded_usize(
            std::env::var("AFL_MAX_STEPS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(MAX_STEPS)
                + 1,
        )
        .max(1);

    let mut map0 = FuzzMap::default();
    let mut map1 = FuzzMap::default();
    let mut key_pool0 = vec![0u8];
    let mut key_pool1 = vec![1u8];
    let mut value_pool0 = vec![0u8, 0xaa];
    let mut value_pool1 = vec![1u8, 1u8 ^ 0xaa];
    let mut sink = 0u64;

    for _ in 0..steps {
        if cursor.is_empty() {
            break;
        }
        run_step(
            &mut map0,
            &mut map1,
            &mut key_pool0,
            &mut key_pool1,
            &mut value_pool0,
            &mut value_pool1,
            &mut cursor,
            &mut sink,
        );
    }

    black_box(sink);
}
