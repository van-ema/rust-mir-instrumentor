use smallvec::SmallVec;

const INLINE_CAP: usize = 8;
const SLOT_COUNT: usize = 2;
const MAX_OPS: usize = 128;
const MAX_TOTAL_LEN: usize = 256;
const MAX_BLOB_LEN: usize = 24;

const CRASH0: &[u8] = &[
    0xff, 0xe9, 0x06, 0x0a, 0x14, 0x1e, 0x28, 0x00, 0x40, 0x00, 0x0d, 0x01, 0x00, 0x02, 0x01,
    0x03, 0x00, 0x04, 0x01, 0xbe, 0xbe, 0xbe, 0x01, 0x00, 0x0e, 0x09, 0xaa, 0x41, 0x00, 0x10,
    0x00,
];

const CRASH1: &[u8] = &[
    0xda, 0xda, 0xda, 0xda, 0x00, 0xf8, 0xfe, 0x7c, 0x7c, 0x7c, 0x7c, 0xff, 0xff, 0xff, 0xfb,
    0x1b, 0x00, 0x3b,
];

#[derive(Clone)]
struct OracleVec {
    buf: [u8; MAX_TOTAL_LEN],
    len: usize,
}

impl Default for OracleVec {
    fn default() -> Self {
        Self {
            buf: [0; MAX_TOTAL_LEN],
            len: 0,
        }
    }
}

impl OracleVec {
    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn truncate(&mut self, new_len: usize) {
        self.len = self.len.min(new_len);
    }

    fn push(&mut self, value: u8) {
        if self.len < MAX_TOTAL_LEN {
            self.buf[self.len] = value;
            self.len += 1;
        }
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            Some(self.buf[self.len])
        }
    }

    fn insert(&mut self, index: usize, value: u8) {
        if self.len >= MAX_TOTAL_LEN {
            return;
        }
        self.buf.copy_within(index..self.len, index + 1);
        self.buf[index] = value;
        self.len += 1;
    }

    fn remove(&mut self, index: usize) -> u8 {
        let value = self.buf[index];
        self.buf.copy_within(index + 1..self.len, index);
        self.len -= 1;
        value
    }

    fn swap_remove(&mut self, index: usize) -> u8 {
        let value = self.buf[index];
        self.len -= 1;
        if index != self.len {
            self.buf[index] = self.buf[self.len];
        }
        value
    }

    fn extend_from_slice(&mut self, slice: &[u8]) {
        let room = MAX_TOTAL_LEN.saturating_sub(self.len);
        let count = slice.len().min(room);
        self.buf[self.len..self.len + count].copy_from_slice(&slice[..count]);
        self.len += count;
    }

    fn insert_from_slice(&mut self, index: usize, slice: &[u8]) {
        let room = MAX_TOTAL_LEN.saturating_sub(self.len);
        let count = slice.len().min(room);
        if count == 0 {
            return;
        }
        self.buf.copy_within(index..self.len, index + count);
        self.buf[index..index + count].copy_from_slice(&slice[..count]);
        self.len += count;
    }

    fn drain_into(&mut self, start: usize, end: usize, out: &mut [u8; MAX_TOTAL_LEN]) -> usize {
        let count = end - start;
        out[..count].copy_from_slice(&self.buf[start..end]);
        self.buf.copy_within(end..self.len, start);
        self.len -= count;
        count
    }

    fn retain_mut(&mut self, plan: &[(u8, bool)]) {
        let mut write = 0usize;
        for (read, (tweak, keep)) in plan.iter().copied().enumerate().take(self.len) {
            let next = self.buf[read].wrapping_add(tweak);
            if keep {
                self.buf[write] = next;
                write += 1;
            }
        }
        self.len = write;
    }

    fn resize(&mut self, new_len: usize, fill: u8) {
        let target = new_len.min(MAX_TOTAL_LEN);
        while self.len < target {
            self.push(fill);
        }
        self.len = target;
    }

    // Original buggy oracle append shape from the old harness.
    fn append(&mut self, other: &mut Self) {
        let room = MAX_TOTAL_LEN.saturating_sub(self.len);
        let count = other.len.min(room);
        let mut tmp = [0u8; MAX_TOTAL_LEN];
        tmp[..count].copy_from_slice(&other.buf[..count]);
        self.extend_from_slice(&tmp[..count]);
        if count < other.len {
            other.buf.copy_within(count..other.len, 0);
        }
        other.len -= count;
    }
}

#[derive(Clone, Default)]
struct Slot {
    sv: SmallVec<u8, INLINE_CAP>,
    model: OracleVec,
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

    fn blob(&mut self, max_len: usize) -> [u8; MAX_BLOB_LEN] {
        let mut out = [0u8; MAX_BLOB_LEN];
        let len = self.bounded_usize(max_len + 1).min(MAX_BLOB_LEN);
        for item in out.iter_mut().take(len) {
            *item = self.byte();
        }
        out
    }

    fn size_hint(&mut self) -> usize {
        match self.byte() % 10 {
            0 => 0,
            1 => 1,
            2 => INLINE_CAP.saturating_sub(1),
            3 => INLINE_CAP,
            4 => INLINE_CAP + 1,
            5 => 2 * INLINE_CAP,
            6 => 3 * INLINE_CAP,
            7 => MAX_BLOB_LEN,
            8 => MAX_TOTAL_LEN / 2,
            _ => self.byte() as usize,
        }
    }
}

fn two_slots_mut(slots: &mut [Slot; SLOT_COUNT], a: usize, b: usize) -> (&mut Slot, &mut Slot) {
    debug_assert!(a != b);
    if a < b {
        let (left, right) = slots.split_at_mut(b);
        (&mut left[a], &mut right[0])
    } else {
        let (left, right) = slots.split_at_mut(a);
        (&mut right[0], &mut left[b])
    }
}

fn check_slot(slot: &Slot) {
    assert_eq!(slot.sv.as_slice(), slot.model.as_slice());
    assert_eq!(slot.sv.len(), slot.model.len());
    assert_eq!(slot.sv.is_empty(), slot.model.is_empty());
}

fn run_step(slots: &mut [Slot; SLOT_COUNT], cursor: &mut Cursor<'_>) {
    let slot_idx = cursor.bounded_usize(SLOT_COUNT);
    let other_idx = 1 - slot_idx;
    let op = cursor.byte() % 17;

    match op {
        0 => {
            if slots[slot_idx].model.len() < MAX_TOTAL_LEN {
                let value = cursor.byte();
                slots[slot_idx].sv.push(value);
                slots[slot_idx].model.push(value);
            }
        }
        1 => {
            assert_eq!(slots[slot_idx].sv.pop(), slots[slot_idx].model.pop());
        }
        2 => {
            if slots[slot_idx].model.len() < MAX_TOTAL_LEN {
                let idx = cursor.bounded_usize(slots[slot_idx].model.len() + 1);
                let value = cursor.byte();
                slots[slot_idx].sv.insert(idx, value);
                slots[slot_idx].model.insert(idx, value);
            }
        }
        3 => {
            if !slots[slot_idx].model.is_empty() {
                let idx = cursor.bounded_usize(slots[slot_idx].model.len());
                let got = slots[slot_idx].sv.remove(idx);
                let expect = slots[slot_idx].model.remove(idx);
                assert_eq!(got, expect);
            }
        }
        4 => {
            if !slots[slot_idx].model.is_empty() {
                let idx = cursor.bounded_usize(slots[slot_idx].model.len());
                let got = slots[slot_idx].sv.swap_remove(idx);
                let expect = slots[slot_idx].model.swap_remove(idx);
                assert_eq!(got, expect);
            }
        }
        5 => {
            let additional = cursor.size_hint().min(MAX_TOTAL_LEN);
            slots[slot_idx].sv.reserve(additional);
        }
        6 => {
            let additional = cursor.size_hint().min(MAX_TOTAL_LEN);
            slots[slot_idx].sv.reserve_exact(additional);
        }
        7 => {
            slots[slot_idx].sv.shrink_to_fit();
        }
        8 => {
            let new_len = cursor.bounded_usize(slots[slot_idx].model.len() + 1);
            slots[slot_idx].sv.truncate(new_len);
            slots[slot_idx].model.truncate(new_len);
        }
        9 => {
            slots[slot_idx].sv.clear();
            slots[slot_idx].model.clear();
        }
        10 => {
            let blob = cursor.blob(MAX_BLOB_LEN);
            let count = cursor.bounded_usize(MAX_BLOB_LEN + 1).min(MAX_BLOB_LEN);
            let chunk = &blob[..count];
            slots[slot_idx].sv.extend_from_slice(chunk);
            slots[slot_idx].model.extend_from_slice(chunk);
        }
        11 => {
            let blob = cursor.blob(MAX_BLOB_LEN);
            let count = cursor.bounded_usize(MAX_BLOB_LEN + 1).min(MAX_BLOB_LEN);
            let chunk = &blob[..count];
            let idx = cursor.bounded_usize(slots[slot_idx].model.len() + 1);
            slots[slot_idx].sv.insert_from_slice_copy(idx, chunk);
            slots[slot_idx].model.insert_from_slice(idx, chunk);
        }
        12 => {
            if !slots[slot_idx].model.is_empty() {
                let start = cursor.bounded_usize(slots[slot_idx].model.len());
                let end = start + cursor.bounded_usize(slots[slot_idx].model.len() - start + 1);
                let consume = cursor.bounded_usize(end - start + 1);

                let mut prefix = [0u8; MAX_TOTAL_LEN];
                let mut prefix_len = 0usize;
                let mut drain = slots[slot_idx].sv.drain(start..end);
                for value in drain.by_ref().take(consume) {
                    prefix[prefix_len] = value;
                    prefix_len += 1;
                }
                drop(drain);

                let mut expected = [0u8; MAX_TOTAL_LEN];
                let expected_len = slots[slot_idx]
                    .model
                    .drain_into(start, end, &mut expected);
                assert_eq!(&prefix[..prefix_len], &expected[..prefix_len.min(expected_len)]);
            }
        }
        13 => {
            let len = slots[slot_idx].model.len();
            let mut plan = [(0u8, false); MAX_TOTAL_LEN];
            for item in plan.iter_mut().take(len) {
                *item = (cursor.byte() & 0x0f, (cursor.byte() & 1) == 0);
            }
            slots[slot_idx].sv.retain_mut({
                let mut idx = 0usize;
                move |value| {
                    let (tweak, keep) = plan[idx];
                    *value = value.wrapping_add(tweak);
                    idx += 1;
                    keep
                }
            });
            slots[slot_idx].model.retain_mut(&plan[..len]);
        }
        14 => {
            let new_len = cursor.size_hint().min(MAX_TOTAL_LEN);
            let fill = cursor.byte();
            slots[slot_idx].sv.resize(new_len, fill);
            slots[slot_idx].model.resize(new_len, fill);
        }
        15 => {
            slots[slot_idx].sv = slots[other_idx].sv.clone();
            slots[slot_idx].model = slots[other_idx].model.clone();
        }
        _ => {
            let (dst, src) = two_slots_mut(slots, slot_idx, other_idx);
            dst.sv.append(&mut src.sv);
            dst.model.append(&mut src.model);
        }
    }

    check_slot(&slots[slot_idx]);
    check_slot(&slots[other_idx]);
}

fn run_input(data: &[u8]) {
    if data.is_empty() {
        return;
    }

    let mut cursor = Cursor::new(data);
    let requested_ops = cursor.byte() as usize;
    let steps = requested_ops.min(MAX_OPS).max(1);
    let mut slots = std::array::from_fn::<_, SLOT_COUNT, _>(|_| Slot::default());

    for _ in 0..steps {
        if cursor.is_empty() {
            break;
        }
        run_step(&mut slots, &mut cursor);
    }

    check_slot(&slots[0]);
    check_slot(&slots[1]);
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "0".to_string());
    match which.as_str() {
        "0" => run_input(CRASH0),
        "1" => run_input(CRASH1),
        "both" => {
            run_input(CRASH0);
            run_input(CRASH1);
        }
        _ => panic!("expected one of: 0, 1, both"),
    }
}
