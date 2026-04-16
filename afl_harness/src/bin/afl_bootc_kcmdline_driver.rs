use bootc_kernel_cmdline::{bytes, utf8};
use std::{env, hint::black_box};

const SLOT_COUNT: usize = 2;
const MAX_STEPS: usize = 64;
const MAX_POOL_LEN: usize = 128;
const MAX_CMDLINE_LEN: usize = 512;

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

fn select_ref<'a, T: ?Sized>(first: &'a T, second: &'a T, idx: usize) -> &'a T {
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

fn mutate_blob(blob: &mut Vec<u8>, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 6 {
        0 => *blob = cursor.take_vec(MAX_POOL_LEN),
        1 => {
            let extra = cursor.bounded_usize(MAX_POOL_LEN.saturating_sub(blob.len()).min(16) + 1);
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

fn blob_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn clamp_bytes_cmdline(slot: &mut bytes::CmdlineOwned) {
    if slot.as_ref().len() > MAX_CMDLINE_LEN {
        let mut v = slot.as_ref().to_vec();
        v.truncate(MAX_CMDLINE_LEN);
        *slot = bytes::CmdlineOwned::from(v);
    }
}

fn clamp_utf8_cmdline(slot: &mut utf8::CmdlineOwned) {
    if slot.len() > MAX_CMDLINE_LEN {
        let mut s = slot.to_string();
        while s.len() > MAX_CMDLINE_LEN {
            s.pop();
        }
        *slot = utf8::CmdlineOwned::from(s);
    }
}

fn choose_existing_bytes_key(
    slot: &bytes::CmdlineOwned,
    cursor: &mut Cursor<'_>,
) -> Option<Vec<u8>> {
    let idx = cursor.bounded_usize(slot.iter().count());
    slot.iter().nth(idx).map(|p| p.key().as_ref().to_vec())
}

fn choose_existing_utf8_key(slot: &utf8::CmdlineOwned, cursor: &mut Cursor<'_>) -> Option<String> {
    let idx = cursor.bounded_usize(slot.iter().count());
    slot.iter().nth(idx).map(|p| p.key().to_string())
}

fn touch_bytes(slot: &bytes::CmdlineOwned, key_hint: &[u8], sink: &mut u64) {
    *sink ^= slot.as_ref().len() as u64;
    for raw in slot.iter_bytes().take(8) {
        *sink ^= raw.len() as u64;
    }
    for param in slot.iter().take(8) {
        *sink ^= param.key().as_ref().len() as u64;
        *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 8);
    }
    for param in slot
        .find_all_starting_with(&key_hint[..key_hint.len().min(4)])
        .take(4)
    {
        *sink ^= param.as_ref().len() as u64;
    }
    if let Some(param) = slot.find(key_hint) {
        *sink ^= param.key().as_ref().len() as u64;
        *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 16);
    }
    *sink ^= slot.value_of(key_hint).map_or(0, |v| v.len() as u64);
    *sink ^= slot
        .require_value_of(key_hint)
        .map_or(0, |v| v.len() as u64) as u64;
    for param in slot.iter_utf8().take(4) {
        *sink ^= param.key().len() as u64;
        *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 24);
    }
}

fn touch_utf8(slot: &utf8::CmdlineOwned, key_hint: &str, sink: &mut u64) {
    *sink ^= slot.len() as u64;
    for raw in slot.iter_str().take(8) {
        *sink ^= raw.len() as u64;
    }
    for param in slot.iter().take(8) {
        *sink ^= param.key().len() as u64;
        *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 8);
    }
    let prefix = &key_hint[..key_hint
        .char_indices()
        .nth(4)
        .map_or(key_hint.len(), |(i, _)| i)];
    for param in slot.find_all_starting_with(&prefix).take(4) {
        *sink ^= param.len() as u64;
    }
    if let Some(param) = slot.find(&key_hint) {
        *sink ^= param.key().len() as u64;
        *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 16);
    }
    *sink ^= slot.value_of(&key_hint).map_or(0, |v| v.len() as u64);
    *sink ^= slot
        .require_value_of(&key_hint)
        .map_or(0, |v| v.len() as u64) as u64;
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    bytes0: &mut bytes::CmdlineOwned,
    bytes1: &mut bytes::CmdlineOwned,
    utf80: &mut utf8::CmdlineOwned,
    utf81: &mut utf8::CmdlineOwned,
    pool0: &mut Vec<u8>,
    pool1: &mut Vec<u8>,
    cursor: &mut Cursor<'_>,
    sink: &mut u64,
) {
    let slot = cursor.bounded_usize(SLOT_COUNT);
    let other = 1 - slot;
    let pool_slot = cursor.bounded_usize(SLOT_COUNT);

    match cursor.byte() % 18 {
        0 => mutate_blob(select_mut(pool0, pool1, pool_slot), cursor),
        1 => {
            let slot_ref = select_mut(bytes0, bytes1, slot);
            *slot_ref = bytes::CmdlineOwned::from(select_ref(&*pool0, &*pool1, pool_slot).clone());
            clamp_bytes_cmdline(slot_ref);
        }
        2 => {
            let param_buf = if (cursor.byte() & 1) == 0 {
                select_ref(&*pool0, &*pool1, pool_slot).clone()
            } else {
                cursor.take_vec(MAX_POOL_LEN)
            };
            if let Some(param) = bytes::Parameter::parse(&param_buf) {
                let slot_ref = select_mut(bytes0, bytes1, slot);
                match cursor.byte() % 2 {
                    0 => {
                        *sink ^= slot_ref.add(&param) as u8 as u64;
                    }
                    _ => {
                        *sink ^= slot_ref.add_or_modify(&param) as u8 as u64;
                    }
                }
                clamp_bytes_cmdline(slot_ref);
            }
        }
        3 => {
            let slot_ref = select_mut(bytes0, bytes1, slot);
            let key = choose_existing_bytes_key(slot_ref, cursor)
                .unwrap_or_else(|| select_ref(&*pool0, &*pool1, pool_slot).clone());
            let key = bytes::ParameterKey::from(&key);
            *sink ^= u64::from(slot_ref.remove(&key));
        }
        4 => {
            let slot_ref = select_mut(bytes0, bytes1, slot);
            let param_buf = if (cursor.byte() & 1) == 0 {
                select_ref(&*pool0, &*pool1, pool_slot).clone()
            } else {
                cursor.take_vec(MAX_POOL_LEN)
            };
            if let Some(param) = bytes::Parameter::parse(&param_buf) {
                *sink ^= u64::from(slot_ref.remove_exact(&param));
            }
        }
        5 => {
            let key_hint = choose_existing_bytes_key(select_ref(&*bytes0, &*bytes1, slot), cursor)
                .unwrap_or_else(|| select_ref(&*pool0, &*pool1, pool_slot).clone());
            touch_bytes(select_ref(&*bytes0, &*bytes1, slot), &key_hint, sink);
        }
        6 => {
            let (dst, src) = select_pair_mut(bytes0, bytes1, slot);
            if (cursor.byte() & 1) == 0 {
                *dst = src.clone();
            } else {
                let additions: Vec<_> = src.iter().take(8).collect();
                dst.extend(additions);
                clamp_bytes_cmdline(dst);
            }
        }
        7 => {
            let src = select_ref(&*bytes0, &*bytes1, slot);
            if let Ok(s) = std::str::from_utf8(src.as_ref()) {
                *select_mut(utf80, utf81, other) = utf8::CmdlineOwned::from(s.to_owned());
            }
        }
        8 => {
            let slot_ref = select_mut(utf80, utf81, slot);
            *slot_ref =
                utf8::CmdlineOwned::from(blob_to_string(select_ref(&*pool0, &*pool1, pool_slot)));
            clamp_utf8_cmdline(slot_ref);
        }
        9 => {
            let text = if (cursor.byte() & 1) == 0 {
                blob_to_string(select_ref(&*pool0, &*pool1, pool_slot))
            } else {
                blob_to_string(&cursor.take_vec(MAX_POOL_LEN))
            };
            if let Some(param) = utf8::Parameter::parse(&text) {
                let slot_ref = select_mut(utf80, utf81, slot);
                match cursor.byte() % 2 {
                    0 => {
                        *sink ^= slot_ref.add(&param) as u8 as u64;
                    }
                    _ => {
                        *sink ^= slot_ref.add_or_modify(&param) as u8 as u64;
                    }
                }
                clamp_utf8_cmdline(slot_ref);
            }
        }
        10 => {
            let slot_ref = select_mut(utf80, utf81, slot);
            let key = choose_existing_utf8_key(slot_ref, cursor)
                .unwrap_or_else(|| blob_to_string(select_ref(&*pool0, &*pool1, pool_slot)));
            let key = utf8::ParameterKey::from(&key);
            *sink ^= u64::from(slot_ref.remove(&key));
        }
        11 => {
            let slot_ref = select_mut(utf80, utf81, slot);
            let text = if (cursor.byte() & 1) == 0 {
                blob_to_string(select_ref(&*pool0, &*pool1, pool_slot))
            } else {
                blob_to_string(&cursor.take_vec(MAX_POOL_LEN))
            };
            if let Some(param) = utf8::Parameter::parse(&text) {
                *sink ^= u64::from(slot_ref.remove_exact(&param));
            }
        }
        12 => {
            let key_hint = choose_existing_utf8_key(select_ref(&*utf80, &*utf81, slot), cursor)
                .unwrap_or_else(|| blob_to_string(select_ref(&*pool0, &*pool1, pool_slot)));
            touch_utf8(select_ref(&*utf80, &*utf81, slot), &key_hint, sink);
        }
        13 => {
            let (dst, src) = select_pair_mut(utf80, utf81, slot);
            if (cursor.byte() & 1) == 0 {
                *dst = src.clone();
            } else {
                let additions: Vec<_> = src.iter().take(8).collect();
                dst.extend(additions);
                clamp_utf8_cmdline(dst);
            }
        }
        14 => {
            let src = select_ref(&*utf80, &*utf81, slot);
            *select_mut(bytes0, bytes1, other) = bytes::CmdlineOwned::from(src.as_bytes().to_vec());
            clamp_bytes_cmdline(select_mut(bytes0, bytes1, other));
        }
        15 => {
            let src = select_ref(&*bytes0, &*bytes1, slot);
            let dst = select_mut(utf80, utf81, other);
            let mut collected = utf8::CmdlineOwned::new();
            let additions: Vec<_> = src.iter_utf8().take(8).collect();
            collected.extend(additions);
            *dst = collected;
        }
        16 => {
            let bytes_slot = select_ref(&*bytes0, &*bytes1, slot);
            let utf8_slot = select_ref(&*utf80, &*utf81, other);
            *sink ^= bytes_slot.iter().count() as u64;
            *sink ^= utf8_slot.iter().count() as u64;
            *sink ^= bytes_slot
                .iter()
                .zip(utf8_slot.iter())
                .take(4)
                .fold(0u64, |acc, (a, b)| {
                    acc ^ (a.key().as_ref().len() as u64) ^ ((b.key().len() as u64) << 8)
                });
        }
        _ => {
            let key = blob_to_string(select_ref(&*pool0, &*pool1, pool_slot));
            if let Some(param) = utf8::Parameter::parse(&key) {
                *sink ^= param.key().len() as u64;
                *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 8);
            }
            if let Some(param) = bytes::Parameter::parse(select_ref(&*pool0, &*pool1, pool_slot)) {
                *sink ^= param.key().as_ref().len() as u64;
                *sink ^= param.value().map_or(0, |v| (v.len() as u64) << 16);
            }
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
            env::var("AFL_MAX_STEPS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(MAX_STEPS)
                + 1,
        )
        .max(1);

    let mut bytes0 = bytes::CmdlineOwned::from(b"quiet splash".to_vec());
    let mut bytes1 = bytes::CmdlineOwned::from(b"console=ttyS0 root=/dev/vda1".to_vec());
    let mut utf80 = utf8::CmdlineOwned::from(String::from("quiet splash"));
    let mut utf81 = utf8::CmdlineOwned::from(String::from("console=ttyS0 root=/dev/vda1"));
    let mut pool0 = b"quiet rd.break root=/dev/vda1".to_vec();
    let mut pool1 = b"console=ttyS0 rootflags=subvol=@ rw".to_vec();
    let mut sink = 0u64;

    for _ in 0..steps {
        if cursor.is_empty() {
            break;
        }
        run_step(
            &mut bytes0,
            &mut bytes1,
            &mut utf80,
            &mut utf81,
            &mut pool0,
            &mut pool1,
            &mut cursor,
            &mut sink,
        );
    }

    black_box(sink);
    black_box(bytes0.as_ref().len() ^ bytes1.as_ref().len() ^ utf80.len() ^ utf81.len());
}
