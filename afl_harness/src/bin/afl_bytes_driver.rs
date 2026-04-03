use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::mem::replace;

const MAX_OPS: usize = 96;
const MAX_CAPACITY: usize = 256;
const MAX_BLOB_LEN: usize = 32;

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

    fn size_hint(&mut self) -> usize {
        match self.byte() % 12 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 7,
            4 => 8,
            5 => 15,
            6 => 16,
            7 => 31,
            8 => 32,
            9 => 63,
            10 => 64,
            _ => self.byte() as usize,
        }
    }

    fn fill_blob(&mut self, out: &mut [u8; MAX_BLOB_LEN]) -> usize {
        let len = self.bounded_usize(MAX_BLOB_LEN + 1);
        for item in out.iter_mut().take(len) {
            *item = self.byte();
        }
        len
    }
}

fn checked_capacity(requested: usize) -> usize {
    requested.min(MAX_CAPACITY)
}

fn run_step(
    mut0: &mut BytesMut,
    mut1: &mut BytesMut,
    frozen0: &mut Bytes,
    frozen1: &mut Bytes,
    cursor: &mut Cursor<'_>,
) {
    let op = cursor.byte() % 18;
    let selector = cursor.byte();
    let use_first_mut = (selector & 1) == 0;
    let use_first_frozen = (selector & 2) == 0;

    match op {
        0 => {
            let cap = checked_capacity(cursor.size_hint());
            let buf = if use_first_mut { mut0 } else { mut1 };
            *buf = BytesMut::with_capacity(cap);
        }
        1 => {
            let mut blob = [0u8; MAX_BLOB_LEN];
            let blob_len = cursor.fill_blob(&mut blob);
            let buf = if use_first_mut { mut0 } else { mut1 };
            let room = MAX_CAPACITY.saturating_sub(buf.len());
            let count = blob_len.min(room);
            buf.extend_from_slice(&blob[..count]);
        }
        2 => {
            let buf = if use_first_mut { mut0 } else { mut1 };
            if buf.len() < MAX_CAPACITY {
                buf.put_u8(cursor.byte());
            }
        }
        3 => {
            let buf = if use_first_mut { mut0 } else { mut1 };
            let additional = checked_capacity(cursor.size_hint())
                .min(MAX_CAPACITY.saturating_sub(buf.len()));
            buf.reserve(additional);
        }
        4 => {
            let buf = if use_first_mut { mut0 } else { mut1 };
            let new_len = cursor.bounded_usize(buf.len() + 1);
            buf.truncate(new_len);
        }
        5 => {
            let buf = if use_first_mut { mut0 } else { mut1 };
            buf.clear();
        }
        6 => {
            if use_first_mut {
                let at = cursor.bounded_usize(mut0.len() + 1);
                *mut1 = mut0.split_to(at);
            } else {
                let at = cursor.bounded_usize(mut1.len() + 1);
                *mut0 = mut1.split_to(at);
            }
        }
        7 => {
            if use_first_mut {
                let at = cursor.bounded_usize(mut0.capacity() + 1);
                *mut1 = mut0.split_off(at);
            } else {
                let at = cursor.bounded_usize(mut1.capacity() + 1);
                *mut0 = mut1.split_off(at);
            }
        }
        8 => {
            if use_first_mut {
                if mut0.len().saturating_add(mut1.len()) <= MAX_CAPACITY {
                    let other = replace(mut1, BytesMut::new());
                    mut0.unsplit(other);
                }
            } else if mut1.len().saturating_add(mut0.len()) <= MAX_CAPACITY {
                let other = replace(mut0, BytesMut::new());
                mut1.unsplit(other);
            }
        }
        9 => {
            if use_first_mut {
                let buf = replace(mut0, BytesMut::new());
                if use_first_frozen {
                    *frozen0 = buf.freeze();
                } else {
                    *frozen1 = buf.freeze();
                }
            } else {
                let buf = replace(mut1, BytesMut::new());
                if use_first_frozen {
                    *frozen0 = buf.freeze();
                } else {
                    *frozen1 = buf.freeze();
                }
            }
        }
        10 => {
            if use_first_frozen {
                *frozen1 = frozen0.clone();
            } else {
                *frozen0 = frozen1.clone();
            }
        }
        11 => {
            if use_first_frozen {
                let at = cursor.bounded_usize(frozen0.len() + 1);
                *frozen1 = frozen0.split_to(at);
            } else {
                let at = cursor.bounded_usize(frozen1.len() + 1);
                *frozen0 = frozen1.split_to(at);
            }
        }
        12 => {
            if use_first_frozen {
                let at = cursor.bounded_usize(frozen0.len() + 1);
                *frozen1 = frozen0.split_off(at);
            } else {
                let at = cursor.bounded_usize(frozen1.len() + 1);
                *frozen0 = frozen1.split_off(at);
            }
        }
        13 => {
            let bytes = if use_first_frozen { frozen0 } else { frozen1 };
            let new_len = cursor.bounded_usize(bytes.len() + 1);
            bytes.truncate(new_len);
        }
        14 => {
            if use_first_frozen {
                let len = frozen0.len();
                let begin = cursor.bounded_usize(len + 1);
                let end = begin + cursor.bounded_usize(len - begin + 1);
                *frozen1 = if (cursor.byte() & 1) == 0 {
                    frozen0.slice(begin..end)
                } else {
                    let subset = &frozen0.as_ref()[begin..end];
                    frozen0.slice_ref(subset)
                };
            } else {
                let len = frozen1.len();
                let begin = cursor.bounded_usize(len + 1);
                let end = begin + cursor.bounded_usize(len - begin + 1);
                *frozen0 = if (cursor.byte() & 1) == 0 {
                    frozen1.slice(begin..end)
                } else {
                    let subset = &frozen1.as_ref()[begin..end];
                    frozen1.slice_ref(subset)
                };
            }
        }
        15 => {
            let bytes = if use_first_frozen {
                frozen0.as_ref()
            } else {
                frozen1.as_ref()
            };
            if use_first_mut {
                *mut0 = BytesMut::from(bytes);
            } else {
                *mut1 = BytesMut::from(bytes);
            }
        }
        16 => {
            if use_first_mut {
                let take_len = cursor.bounded_usize(mut0.len() + 1);
                if use_first_frozen {
                    *frozen0 = mut0.copy_to_bytes(take_len);
                } else {
                    *frozen1 = mut0.copy_to_bytes(take_len);
                }
            } else {
                let take_len = cursor.bounded_usize(mut1.len() + 1);
                if use_first_frozen {
                    *frozen0 = mut1.copy_to_bytes(take_len);
                } else {
                    *frozen1 = mut1.copy_to_bytes(take_len);
                }
            }
        }
        _ => {
            let fill = cursor.byte();
            let buf = if use_first_mut { mut0 } else { mut1 };
            let additional = checked_capacity(cursor.size_hint())
                .min(MAX_CAPACITY.saturating_sub(buf.len()));
            buf.reserve(additional);
            let room = buf.spare_capacity_mut().len();
            let write_len = room.min(additional).min(MAX_BLOB_LEN);
            if write_len != 0 {
                let spare = buf.spare_capacity_mut();
                for cell in spare.iter_mut().take(write_len) {
                    cell.write(fill);
                }
                unsafe {
                    buf.advance_mut(write_len);
                }
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
    let requested_steps = cursor.byte() as usize;
    let max_steps = std::env::var("AFL_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_OPS);
    let steps = requested_steps.min(max_steps).max(1);

    let mut mut0 = BytesMut::new();
    let mut mut1 = BytesMut::new();
    let mut frozen0 = Bytes::new();
    let mut frozen1 = Bytes::new();

    for _ in 0..steps {
        if cursor.is_empty() {
            break;
        }
        run_step(&mut mut0, &mut mut1, &mut frozen0, &mut frozen1, &mut cursor);
    }
}
