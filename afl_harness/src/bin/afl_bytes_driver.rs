use bytes::{Buf, BufMut, Bytes, BytesMut};

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut idx = 0usize;
    let cap = (data[idx] as usize).max(1);
    idx += 1;

    let mut buf = BytesMut::with_capacity(cap);
    let mut frozen: Option<Bytes> = None;

    while idx < data.len() {
        let op = data[idx] % 4;
        idx += 1;

        match op {
            0 => {
                if idx >= data.len() {
                    break;
                }
                let take = (data[idx] as usize).min(64);
                idx += 1;
                let end = (idx + take).min(data.len());
                if end > idx {
                    buf.put_slice(&data[idx..end]);
                    idx = end;
                }
            }
            1 => {
                if idx + 4 <= data.len() {
                    let value = u32::from_le_bytes([data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]);
                    buf.put_u32(value);
                    idx += 4;
                } else {
                    break;
                }
            }
            2 => {
                if !buf.is_empty() {
                    let view = buf.clone().freeze();
                    let mut cursor = view.clone();
                    let consume = (data[idx.saturating_sub(1)] as usize) % (cursor.remaining() + 1);
                    if consume > 0 {
                        let _ = cursor.copy_to_bytes(consume);
                    }
                    frozen = Some(view);
                }
            }
            _ => {
                if let Some(bytes) = frozen.as_ref() {
                    let mut view = bytes.clone();
                    if view.has_remaining() {
                        let n = (data[idx.saturating_sub(1)] as usize) % (view.remaining() + 1);
                        if n > 0 {
                            let _ = view.copy_to_bytes(n);
                        }
                    }
                } else if !buf.is_empty() {
                    let frozen_local = buf.clone().freeze();
                    let mut view = frozen_local.clone();
                    if view.has_remaining() {
                        let n = (data[idx.saturating_sub(1)] as usize) % (view.remaining() + 1);
                        if n > 0 {
                            let _ = view.copy_to_bytes(n);
                        }
                    }
                    let mut buf2 = BytesMut::from(&frozen_local[..]);
                    if idx < data.len() {
                        buf2.put_u16(data[idx] as u16);
                    }
                    frozen = Some(buf2.freeze());
                }
            }
        }
    }
}
