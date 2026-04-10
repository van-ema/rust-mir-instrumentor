use bumpalo::{
    collections::{String as BumpString, Vec as BumpVec},
    Bump,
};
use std::{alloc::Layout, env, hint::black_box, ptr};

const SLOT_COUNT: usize = 2;
const POOL_SLOTS: usize = 2;
const DEFAULT_MAX_STEPS: usize = 96;
const MAX_BLOB_LEN: usize = 96;
const MAX_TEXT_LEN: usize = 96;
const MAX_ARENA_CAPACITY: usize = 1024;
const MAX_ALLOC_LEN: usize = 128;
const MAX_TEMP_OPS: usize = 24;

struct Cursor<'a> {
    data: &'a [u8],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, idx: 0 }
    }

    fn byte(&mut self) -> u8 {
        if self.idx >= self.data.len() {
            0
        } else {
            let value = self.data[self.idx];
            self.idx += 1;
            value
        }
    }

    fn bounded_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.byte() as usize) % bound
        }
    }

    fn size_hint(&mut self) -> usize {
        match self.byte() % 12 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            5 => 16,
            6 => 24,
            7 => 32,
            8 => 48,
            9 => 64,
            10 => 96,
            _ => self.byte() as usize,
        }
    }

    fn ascii_char(&mut self) -> char {
        let value = 0x20u8 + (self.byte() % 0x5f);
        value as char
    }
}

fn max_steps() -> usize {
    env::var("AFL_MAX_STEPS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_STEPS)
}

fn collections_enabled() -> bool {
    matches!(
        env::var("BUMPALO_COLLECTIONS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn bounded_len(cursor: &mut Cursor<'_>, max_len: usize) -> usize {
    cursor.bounded_usize(max_len + 1)
}

fn pick_align(cursor: &mut Cursor<'_>) -> usize {
    match cursor.byte() % 5 {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        _ => 16,
    }
}

fn mutate_blob(blob: &mut Vec<u8>, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 6 {
        0 => {
            blob.clear();
            let len = bounded_len(cursor, MAX_BLOB_LEN);
            blob.reserve(len);
            for _ in 0..len {
                blob.push(cursor.byte());
            }
        }
        1 => {
            let room = MAX_BLOB_LEN.saturating_sub(blob.len());
            let extra = bounded_len(cursor, room.min(16));
            for _ in 0..extra {
                blob.push(cursor.byte());
            }
        }
        2 => {
            let new_len = bounded_len(cursor, blob.len());
            blob.truncate(new_len);
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

fn mutate_text(text: &mut String, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 6 {
        0 => {
            text.clear();
            let len = bounded_len(cursor, MAX_TEXT_LEN);
            text.reserve(len);
            for _ in 0..len {
                text.push(cursor.ascii_char());
            }
        }
        1 => {
            let room = MAX_TEXT_LEN.saturating_sub(text.len());
            let extra = bounded_len(cursor, room.min(16));
            for _ in 0..extra {
                text.push(cursor.ascii_char());
            }
        }
        2 => {
            let new_len = bounded_len(cursor, text.len());
            text.truncate(new_len);
        }
        3 => {
            if !text.is_empty() {
                let idx = cursor.bounded_usize(text.len());
                text.remove(idx);
            }
        }
        4 => {
            let idx = cursor.bounded_usize(text.len() + 1);
            text.insert(idx, cursor.ascii_char());
            if text.len() > MAX_TEXT_LEN {
                text.truncate(MAX_TEXT_LEN);
            }
        }
        _ => {
            if !text.is_empty() {
                let at = cursor.bounded_usize(text.len());
                let tail: String = text[at..].chars().rev().collect();
                text.truncate(at);
                let room = MAX_TEXT_LEN.saturating_sub(text.len());
                text.push_str(&tail[..tail.len().min(room)]);
            }
        }
    }
}

fn direct_alloc_ops(bump: &Bump, blob: &[u8], text: &str, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 8 {
        0 => {
            let fill = cursor.byte();
            let bytes = bump.alloc([fill; 32]);
            let idx = cursor.bounded_usize(bytes.len());
            bytes[idx] ^= cursor.byte();
            black_box(bytes.as_ptr());
        }
        1 => {
            if let Ok(value) = bump.try_alloc([cursor.byte(); 16]) {
                let idx = cursor.bounded_usize(value.len());
                value[idx] ^= cursor.byte();
                black_box(value.as_ptr());
            }
        }
        2 => {
            let value = bump.alloc_with(|| (cursor.byte(), cursor.byte(), cursor.byte(), cursor.byte()));
            value.0 ^= cursor.byte();
            black_box(value.2);
        }
        3 => {
            let _ = bump.try_alloc_with(|| [cursor.byte(); 24]).map(|value| {
                let idx = cursor.bounded_usize(value.len());
                value[idx] = value[idx].wrapping_add(cursor.byte());
                black_box(value.as_ptr());
            });
        }
        4 => {
            let _ = bump.alloc_try_with(|| {
                if (cursor.byte() & 1) == 0 {
                    Ok([cursor.byte(); 12])
                } else {
                    Err(cursor.byte())
                }
            });
        }
        5 => {
            let marker = bump.alloc((blob.len(), text.len(), cursor.byte()));
            black_box(marker.0 ^ marker.1);
        }
        6 => {
            let len = 1 + bounded_len(cursor, 24);
            let mut total = 0usize;
            for _ in 0..len {
                let cell = bump.alloc(cursor.byte());
                total = total.wrapping_add(*cell as usize);
            }
            black_box(total);
        }
        _ => {
            let len = bounded_len(cursor, 32);
            let slice = bump.alloc_slice_fill_copy(len, cursor.byte());
            if !slice.is_empty() {
                let idx = cursor.bounded_usize(slice.len());
                slice[idx] ^= cursor.byte();
            }
            black_box(slice.len());
        }
    }
}

fn slice_alloc_ops(bump: &Bump, blob: &[u8], text: &str, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 8 {
        0 => {
            let slice = bump.alloc_slice_copy(blob);
            black_box(slice.len());
        }
        1 => {
            let _ = bump.try_alloc_slice_copy(blob).map(|slice| black_box(slice.len()));
        }
        2 => {
            let slice = bump.alloc_slice_clone(blob);
            black_box(slice.len());
        }
        3 => {
            let _ = bump.try_alloc_slice_clone(blob).map(|slice| black_box(slice.len()));
        }
        4 => {
            let s = bump.alloc_str(text);
            black_box(s.len());
        }
        5 => {
            let _ = bump.try_alloc_str(text).map(|s| black_box(s.len()));
        }
        6 => {
            let joined = bump.alloc_slice_fill_iter(blob.iter().take(blob.len().min(16)).copied());
            black_box(joined.len());
        }
        _ => {
            let fill_len = bounded_len(cursor, MAX_ALLOC_LEN);
            let fill = bump.alloc_slice_fill_with(fill_len, |_| cursor.byte());
            black_box(fill.len());
        }
    }
}

fn fill_alloc_ops(bump: &Bump, blob: &[u8], cursor: &mut Cursor<'_>) {
    match cursor.byte() % 5 {
        0 => {
            let len = bounded_len(cursor, MAX_ALLOC_LEN);
            let slice = bump.alloc_slice_fill_copy(len, cursor.byte());
            black_box(slice.len());
        }
        1 => {
            let len = bounded_len(cursor, MAX_ALLOC_LEN / 2);
            let template = [cursor.byte(), cursor.byte(), cursor.byte(), cursor.byte()];
            let slice = bump.alloc_slice_fill_clone(len, &template);
            black_box(slice.len());
        }
        2 => {
            let len = bounded_len(cursor, MAX_ALLOC_LEN / 2);
            let slice = bump.alloc_slice_fill_default::<u32>(len);
            black_box(slice.len());
        }
        3 => {
            let _ = bump.try_alloc_slice_fill_iter(blob.iter().copied().map(|byte| byte ^ cursor.byte()));
        }
        _ => {
            let len = bounded_len(cursor, MAX_ALLOC_LEN / 2);
            let _ = bump.try_alloc_slice_fill_with(len, |_| cursor.byte()).map(|slice| black_box(slice.len()));
        }
    }
}

fn layout_alloc_ops(bump: &Bump, cursor: &mut Cursor<'_>) {
    let size = 1 + bounded_len(cursor, MAX_ALLOC_LEN);
    let align = pick_align(cursor);
    if let Ok(layout) = Layout::from_size_align(size, align) {
        match cursor.byte() % 3 {
            0 => {
                let ptr = bump.alloc_layout(layout);
                unsafe {
                    ptr::write_bytes(ptr.as_ptr(), cursor.byte(), size);
                }
                black_box(ptr.as_ptr());
            }
            _ => {
                let _ = bump.try_alloc_layout(layout).map(|ptr| {
                    unsafe {
                        ptr::write_bytes(ptr.as_ptr(), cursor.byte(), size);
                    }
                    black_box(ptr.as_ptr());
                });
            }
        }
    }
}

fn vec_ops(bump: &Bump, blob_a: &[u8], blob_b: &[u8], cursor: &mut Cursor<'_>) {
    let cap = bounded_len(cursor, MAX_ALLOC_LEN);
    let mut vec = BumpVec::with_capacity_in(cap, bump);
    let seed_len = blob_a.len().min(MAX_ALLOC_LEN / 2);
    vec.extend_from_slice_copy(&blob_a[..seed_len]);

    let temp_ops = 1 + cursor.bounded_usize(MAX_TEMP_OPS);
    for _ in 0..temp_ops {
        match cursor.byte() % 12 {
            0 => {
                if vec.len() < MAX_ALLOC_LEN {
                    vec.push(cursor.byte());
                }
            }
            1 => {
                let extra = bounded_len(cursor, 16).min(MAX_ALLOC_LEN.saturating_sub(vec.len()));
                vec.reserve(extra);
            }
            2 => {
                let extra = bounded_len(cursor, 16).min(MAX_ALLOC_LEN.saturating_sub(vec.len()));
                vec.reserve_exact(extra);
            }
            3 => {
                let new_len = bounded_len(cursor, vec.len());
                vec.truncate(new_len);
            }
            4 => {
                if !vec.is_empty() {
                    let idx = cursor.bounded_usize(vec.len());
                    black_box(vec.swap_remove(idx));
                }
            }
            5 => {
                if !vec.is_empty() {
                    let idx = cursor.bounded_usize(vec.len());
                    black_box(vec.remove(idx));
                }
            }
            6 => {
                if vec.len() < MAX_ALLOC_LEN {
                    let idx = cursor.bounded_usize(vec.len() + 1);
                    vec.insert(idx, cursor.byte());
                }
            }
            7 => {
                let at = cursor.bounded_usize(vec.len() + 1);
                let mut tail = vec.split_off(at);
                if (cursor.byte() & 1) == 0 {
                    vec.append(&mut tail);
                } else {
                    let consumed: usize = tail.drain(..).map(|byte| byte as usize).sum();
                    black_box(consumed);
                }
            }
            8 => {
                let mut other = BumpVec::new_in(bump);
                let other_len = blob_b.len().min(16);
                other.extend_from_slice_copy(&blob_b[..other_len]);
                vec.append(&mut other);
                if vec.len() > MAX_ALLOC_LEN {
                    vec.truncate(MAX_ALLOC_LEN);
                }
            }
            9 => {
                let end = bounded_len(cursor, vec.len());
                let sum: usize = vec.drain(..end).map(|byte| byte as usize).sum();
                black_box(sum);
            }
            10 => {
                vec.retain_mut(|byte| {
                    if (*byte & 1) == 0 {
                        *byte ^= cursor.byte();
                    }
                    (*byte as usize) % 3 != 0
                });
            }
            _ => {
                let slices: [&[u8]; 2] = [blob_a, blob_b];
                let refs: Vec<&[u8]> = slices
                    .into_iter()
                    .map(|slice| &slice[..slice.len().min(8)])
                    .collect();
                vec.extend_from_slices_copy(&refs);
                if vec.len() > MAX_ALLOC_LEN {
                    vec.truncate(MAX_ALLOC_LEN);
                }
                vec.dedup();
            }
        }
    }

    match cursor.byte() % 3 {
        0 => {
            let slice = vec.into_bump_slice_mut();
            if !slice.is_empty() {
                let idx = cursor.bounded_usize(slice.len());
                slice[idx] ^= cursor.byte();
            }
            black_box(slice.len());
        }
        1 => {
            let sum: usize = vec.into_iter().map(|byte| byte as usize).sum();
            black_box(sum);
        }
        _ => {
            black_box(vec.len());
        }
    }
}

fn string_ops(bump: &Bump, text: &str, cursor: &mut Cursor<'_>) {
    let cap = bounded_len(cursor, MAX_TEXT_LEN);
    let mut string = if (cursor.byte() & 1) == 0 {
        BumpString::with_capacity_in(cap, bump)
    } else {
        BumpString::from_str_in(text, bump)
    };

    let temp_ops = 1 + cursor.bounded_usize(MAX_TEMP_OPS);
    for _ in 0..temp_ops {
        match cursor.byte() % 10 {
            0 => {
                if string.len() < MAX_TEXT_LEN {
                    string.push(cursor.ascii_char());
                }
            }
            1 => {
                let extra = bounded_len(cursor, 12).min(MAX_TEXT_LEN.saturating_sub(string.len()));
                string.reserve(extra);
            }
            2 => {
                let room = MAX_TEXT_LEN.saturating_sub(string.len());
                if room != 0 {
                    let count = bounded_len(cursor, room.min(12));
                    for _ in 0..count {
                        string.push(cursor.ascii_char());
                    }
                }
            }
            3 => {
                let new_len = bounded_len(cursor, string.len());
                string.truncate(new_len);
            }
            4 => {
                let _ = string.pop();
            }
            5 => {
                if !string.is_empty() {
                    let idx = cursor.bounded_usize(string.len());
                    black_box(string.remove(idx));
                }
            }
            6 => {
                if string.len() < MAX_TEXT_LEN {
                    let idx = cursor.bounded_usize(string.len() + 1);
                    string.insert(idx, cursor.ascii_char());
                }
            }
            7 => {
                if string.len() < MAX_TEXT_LEN {
                    let idx = cursor.bounded_usize(string.len() + 1);
                    let mut buf = [0u8; 8];
                    let str_len = bounded_len(cursor, buf.len());
                    for byte in buf.iter_mut().take(str_len) {
                        *byte = 0x20 + (cursor.byte() % 0x5f);
                    }
                    if let Ok(fragment) = std::str::from_utf8(&buf[..str_len]) {
                        string.insert_str(idx, fragment);
                        if string.len() > MAX_TEXT_LEN {
                            string.truncate(MAX_TEXT_LEN);
                        }
                    }
                }
            }
            8 => {
                let at = cursor.bounded_usize(string.len() + 1);
                let tail = string.split_off(at);
                let room = MAX_TEXT_LEN.saturating_sub(string.len());
                string.push_str(&tail[..tail.len().min(room)]);
            }
            _ => {
                let end = bounded_len(cursor, string.len());
                let drained: usize = string.drain(..end).map(|ch| ch as usize).sum();
                black_box(drained);
            }
        }
    }

    black_box(string.len());
}

fn chunk_ops(bump: &mut Bump, cursor: &mut Cursor<'_>) {
    let before = bump.allocated_bytes();
    let count = 1 + cursor.bounded_usize(8);
    for _ in 0..count {
        let len = 1 + bounded_len(cursor, 24);
        let _ = bump.try_alloc_slice_fill_copy(len, cursor.byte());
    }
    let chunk_bytes: usize = bump.iter_allocated_chunks().map(|chunk| chunk.len()).sum();
    black_box(before);
    black_box(chunk_bytes);
    black_box(bump.chunk_capacity());
    black_box(bump.allocated_bytes());
}

fn rollover_ops(bump: &Bump, blob: &[u8], cursor: &mut Cursor<'_>) {
    let count = 1 + cursor.bounded_usize(24);
    let mut total = 0usize;
    for _ in 0..count {
        let len = 1 + bounded_len(cursor, blob.len().max(1).min(32));
        let end = len.min(blob.len());
        let src = &blob[..end];
        if let Ok(slice) = bump.try_alloc_slice_copy(src) {
            total = total.wrapping_add(slice.len());
        }
        if let Ok(layout) = Layout::from_size_align(1 + bounded_len(cursor, 16), pick_align(cursor)) {
            if let Ok(ptr) = bump.try_alloc_layout(layout) {
                unsafe {
                    ptr::write_bytes(ptr.as_ptr(), cursor.byte(), layout.size());
                }
                total ^= layout.size();
            }
        }
    }
    black_box(total);
}

fn run_step(
    bumps: &mut [Bump; SLOT_COUNT],
    blobs: &mut [Vec<u8>; POOL_SLOTS],
    texts: &mut [String; POOL_SLOTS],
    cursor: &mut Cursor<'_>,
) {
    let op = cursor.byte() % 16;
    let slot = cursor.bounded_usize(SLOT_COUNT);
    let pool_a = cursor.bounded_usize(POOL_SLOTS);
    let pool_b = 1 - pool_a;
    let use_collections = collections_enabled();

    match op {
        0 => mutate_blob(&mut blobs[pool_a], cursor),
        1 => mutate_text(&mut texts[pool_a], cursor),
        2 => {
            let capacity = cursor.size_hint().min(MAX_ARENA_CAPACITY);
            bumps[slot] = match cursor.byte() % 3 {
                0 => Bump::new(),
                1 => Bump::with_capacity(capacity),
                _ => Bump::try_with_capacity(capacity).unwrap_or_else(|_| Bump::new()),
            };
        }
        3 => bumps[slot].reset(),
        4 => direct_alloc_ops(&bumps[slot], &blobs[pool_a], &texts[pool_a], cursor),
        5 => slice_alloc_ops(&bumps[slot], &blobs[pool_a], &texts[pool_a], cursor),
        6 => fill_alloc_ops(&bumps[slot], &blobs[pool_a], cursor),
        7 => layout_alloc_ops(&bumps[slot], cursor),
        8 => {
            if use_collections {
                vec_ops(&bumps[slot], &blobs[pool_a], &blobs[pool_b], cursor);
            } else {
                rollover_ops(&bumps[slot], &blobs[pool_a], cursor);
            }
        }
        9 => {
            if use_collections {
                string_ops(&bumps[slot], &texts[pool_a], cursor);
            } else {
                direct_alloc_ops(&bumps[slot], &blobs[pool_a], &texts[pool_a], cursor);
            }
        }
        10 => chunk_ops(&mut bumps[slot], cursor),
        11 => rollover_ops(&bumps[slot], &blobs[pool_a], cursor),
        12 => {
            if use_collections {
                let prefix_len = bounded_len(cursor, texts[pool_a].len());
                let prefix = &texts[pool_a][..prefix_len];
                let joined = BumpString::from_str_in(prefix, &bumps[slot]);
                black_box(joined.len());
                let bytes = bumps[slot].alloc_slice_fill_iter(prefix.bytes());
                black_box(bytes.len());
            } else {
                slice_alloc_ops(&bumps[slot], &blobs[pool_a], &texts[pool_a], cursor);
            }
        }
        13 => {
            blobs[pool_b] = blobs[pool_a].clone();
            if blobs[pool_b].len() > MAX_BLOB_LEN {
                blobs[pool_b].truncate(MAX_BLOB_LEN);
            }
        }
        14 => texts[pool_b] = texts[pool_a].clone(),
        _ => {
            black_box(bumps[slot].allocated_bytes());
            black_box(blobs[pool_a].len());
            black_box(texts[pool_a].len());
        }
    }
}

fn main() {
    let path = env::args().nth(1).expect("missing input path");
    let data = std::fs::read(path).expect("failed to read input");
    let mut cursor = Cursor::new(&data);

    let mut bumps = [Bump::new(), Bump::new()];
    let mut blobs = [vec![0], vec![1, 2, 3, 4]];
    let mut texts = [String::from("b"), String::from("bump")];

    let steps = 1 + cursor.bounded_usize(max_steps());
    for _ in 0..steps {
        run_step(&mut bumps, &mut blobs, &mut texts, &mut cursor);
    }

    black_box(bumps[0].allocated_bytes());
    black_box(bumps[1].allocated_bytes());
}
