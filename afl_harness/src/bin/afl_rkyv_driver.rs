use rkyv::{rancor::Error, util::AlignedVec, Archive, Deserialize, Serialize};
use std::hint::black_box;

const SLOT_COUNT: usize = 2;
const MAX_STEPS: usize = 96;
const MAX_BLOB_LEN: usize = 96;
const MAX_TEXT_LEN: usize = 64;
const MAX_TAGS: usize = 12;
const MAX_CHUNKS: usize = 6;
const MAX_CHUNK_LEN: usize = 24;

#[derive(Archive, Deserialize, Serialize, Debug, Clone, Default)]
struct FuzzRecord {
    header: u32,
    flags: u32,
    tail: [u8; 8],
    blob: Vec<u8>,
    text: String,
    tags: Vec<u32>,
    chunks: Vec<Vec<u8>>,
}

#[derive(Default)]
struct BufferSlot {
    bytes: AlignedVec,
    valid: bool,
}

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

    fn u32(&mut self) -> u32 {
        let mut out = 0u32;
        for shift in 0..4 {
            out |= (self.byte() as u32) << (shift * 8);
        }
        out
    }

    fn bounded_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.byte() as usize) % bound
        }
    }

    fn take_vec(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.byte());
        }
        out
    }
}

fn build_record(cursor: &mut Cursor<'_>) -> FuzzRecord {
    let header = cursor.u32();
    let flags = cursor.u32();
    let mut tail = [0u8; 8];
    for byte in &mut tail {
        *byte = cursor.byte();
    }

    let blob_len = cursor.bounded_usize(MAX_BLOB_LEN + 1);
    let blob = cursor.take_vec(blob_len);
    let text_len = cursor.bounded_usize(MAX_TEXT_LEN + 1);
    let text = String::from_utf8_lossy(&cursor.take_vec(text_len)).into_owned();

    let mut tags = Vec::with_capacity(cursor.bounded_usize(MAX_TAGS + 1));
    let tags_len = tags.capacity();
    for _ in 0..tags_len {
        tags.push(cursor.u32());
    }

    let mut chunks = Vec::with_capacity(cursor.bounded_usize(MAX_CHUNKS + 1));
    let chunks_len = chunks.capacity();
    for _ in 0..chunks_len {
        let chunk_len = cursor.bounded_usize(MAX_CHUNK_LEN + 1);
        chunks.push(cursor.take_vec(chunk_len));
    }

    FuzzRecord {
        header,
        flags,
        tail,
        blob,
        text,
        tags,
        chunks,
    }
}

fn mutate_record(record: &mut FuzzRecord, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 8 {
        0 => {
            record.header = record.header.wrapping_add(cursor.u32());
            record.flags ^= cursor.u32();
        }
        1 => {
            let idx = cursor.bounded_usize(record.tail.len());
            record.tail[idx] ^= cursor.byte();
        }
        2 => {
            if record.blob.len() < MAX_BLOB_LEN && (cursor.byte() & 1) == 0 {
                record.blob.push(cursor.byte());
            } else if !record.blob.is_empty() {
                let new_len = cursor.bounded_usize(record.blob.len());
                record.blob.truncate(new_len);
            }
        }
        3 => {
            let extra_len = cursor.bounded_usize(8);
            let extra = cursor.take_vec(extra_len);
            if record.text.len() + extra.len() <= MAX_TEXT_LEN {
                record.text.push_str(&String::from_utf8_lossy(&extra));
            } else {
                record.text.truncate(cursor.bounded_usize(record.text.len() + 1));
            }
        }
        4 => {
            if record.tags.len() < MAX_TAGS && (cursor.byte() & 1) == 0 {
                record.tags.push(cursor.u32());
            } else if !record.tags.is_empty() {
                let idx = cursor.bounded_usize(record.tags.len());
                record.tags[idx] ^= cursor.u32();
            }
        }
        5 => {
            if record.chunks.len() < MAX_CHUNKS && (cursor.byte() & 1) == 0 {
                record
                    .chunks
                    .push({
                        let chunk_len = cursor.bounded_usize(MAX_CHUNK_LEN + 1);
                        cursor.take_vec(chunk_len)
                    });
            } else if !record.chunks.is_empty() {
                let idx = cursor.bounded_usize(record.chunks.len());
                if record.chunks[idx].is_empty() {
                    record.chunks[idx].push(cursor.byte());
                } else {
                    let inner = cursor.bounded_usize(record.chunks[idx].len());
                    record.chunks[idx][inner] ^= cursor.byte();
                }
            }
        }
        6 => {
            record.blob = record.text.as_bytes().to_vec();
            record.text = String::from_utf8_lossy(&record.blob).into_owned();
        }
        _ => {
            record.tags.reverse();
            record.chunks.reverse();
        }
    }
}

fn touch_archived(record: &ArchivedFuzzRecord) -> u64 {
    let mut acc = record.header.to_native() as u64 ^ ((record.flags.to_native() as u64) << 17);

    for &byte in record.tail.iter() {
        acc = acc.rotate_left(7) ^ byte as u64;
    }
    for &byte in record.blob.as_slice().iter().take(16) {
        acc = acc.rotate_left(5) ^ byte as u64;
    }
    for &byte in record.text.as_str().as_bytes().iter().take(16) {
        acc = acc.rotate_left(3) ^ byte as u64;
    }
    for tag in record.tags.iter().take(8) {
        acc = acc.wrapping_add(tag.to_native() as u64).rotate_left(9);
    }
    for chunk in record.chunks.iter().take(4) {
        acc ^= chunk.len() as u64;
        for &byte in chunk.as_slice().iter().take(8) {
            acc = acc.wrapping_mul(0x100_0000_01b3).wrapping_add(byte as u64);
        }
    }

    acc ^= record.blob.len() as u64;
    acc ^= record.text.as_str().len() as u64;
    acc ^= record.tags.len() as u64;
    acc ^ ((record.chunks.len() as u64) << 32)
}

fn serialize_slot(record: &FuzzRecord, slot: &mut BufferSlot) {
    if let Ok(bytes) = rkyv::to_bytes::<Error>(record) {
        slot.bytes = bytes;
        slot.valid = true;
    }
}

fn checked_access(slot: &BufferSlot, sink: &mut u64) {
    if let Ok(archived) = rkyv::access::<ArchivedFuzzRecord, Error>(slot.bytes.as_slice()) {
        *sink ^= touch_archived(archived);
    }
}

fn checked_deserialize(slot: &BufferSlot, sink: &mut u64) {
    if let Ok(value) = rkyv::from_bytes::<FuzzRecord, Error>(slot.bytes.as_slice()) {
        *sink ^= value.header as u64;
        *sink ^= value.blob.len() as u64;
        *sink ^= value.text.len() as u64;
        *sink ^= value.tags.len() as u64;
        *sink ^= value.chunks.len() as u64;
    }
}

fn unchecked_access(slot: &BufferSlot, sink: &mut u64) {
    if !slot.valid {
        return;
    }
    let archived = unsafe { rkyv::access_unchecked::<ArchivedFuzzRecord>(slot.bytes.as_slice()) };
    *sink ^= touch_archived(archived);
}

fn unchecked_deserialize(slot: &BufferSlot, sink: &mut u64) {
    if !slot.valid {
        return;
    }
    if let Ok(value) = unsafe { rkyv::from_bytes_unchecked::<FuzzRecord, Error>(slot.bytes.as_slice()) } {
        *sink = sink.wrapping_add(value.header as u64);
        *sink ^= value.text.len() as u64;
    }
}

fn checked_mutate(slot: &mut BufferSlot, cursor: &mut Cursor<'_>, sink: &mut u64) {
    if !slot.valid {
        return;
    }
    if let Ok(archived) = rkyv::access_mut::<ArchivedFuzzRecord, Error>(slot.bytes.as_mut_slice()) {
        *sink ^= touch_archived(&*archived);
        *sink ^= cursor.u32() as u64;
    }
}

fn corrupt_buffer(slot: &mut BufferSlot, cursor: &mut Cursor<'_>) {
    match cursor.byte() % 5 {
        0 => {
            if !slot.bytes.is_empty() {
                let idx = cursor.bounded_usize(slot.bytes.len());
                slot.bytes.as_mut_slice()[idx] ^= cursor.byte();
            }
        }
        1 => {
            if !slot.bytes.is_empty() {
                let start = cursor.bounded_usize(slot.bytes.len());
                let end = start + cursor.bounded_usize(slot.bytes.len() - start + 1);
                slot.bytes.as_mut_slice().copy_within(start..end, 0);
            }
        }
        2 => {
            let new_len = cursor.bounded_usize(MAX_BLOB_LEN + MAX_TEXT_LEN + 32);
            slot.bytes.resize(new_len, cursor.byte());
        }
        3 => {
            slot.bytes.push(cursor.byte());
            slot.bytes.push(cursor.byte());
        }
        _ => {
            let extra_len = cursor.bounded_usize(8);
            let extra = cursor.take_vec(extra_len);
            slot.bytes.extend_from_slice(&extra);
        }
    }
    slot.valid = false;
}

fn run_step(
    records: &mut [FuzzRecord; SLOT_COUNT],
    buffers: &mut [BufferSlot; SLOT_COUNT],
    cursor: &mut Cursor<'_>,
    sink: &mut u64,
) {
    let slot = cursor.bounded_usize(SLOT_COUNT);
    let other = 1 - slot;

    match cursor.byte() % 12 {
        0 => records[slot] = build_record(cursor),
        1 => mutate_record(&mut records[slot], cursor),
        2 => records[slot] = records[other].clone(),
        3 => serialize_slot(&records[slot], &mut buffers[slot]),
        4 => checked_access(&buffers[slot], sink),
        5 => checked_deserialize(&buffers[slot], sink),
        6 => checked_mutate(&mut buffers[slot], cursor, sink),
        7 => unchecked_access(&buffers[slot], sink),
        8 => unchecked_deserialize(&buffers[slot], sink),
        9 => corrupt_buffer(&mut buffers[slot], cursor),
        10 => {
            buffers[slot].bytes = buffers[other].bytes.clone();
            buffers[slot].valid = buffers[other].valid;
        }
        _ => {
            if let Ok(value) = rkyv::from_bytes::<FuzzRecord, Error>(buffers[slot].bytes.as_slice()) {
                records[other] = value;
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
            std::env::var("AFL_MAX_STEPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_STEPS)
                + 1,
        )
        .max(1);

    let mut records = std::array::from_fn::<_, SLOT_COUNT, _>(|_| FuzzRecord::default());
    let mut buffers = std::array::from_fn::<_, SLOT_COUNT, _>(|_| BufferSlot::default());
    let mut sink = 0u64;

    for _ in 0..steps {
        if cursor.is_empty() {
            break;
        }
        run_step(&mut records, &mut buffers, &mut cursor, &mut sink);
    }

    black_box(sink);
}
