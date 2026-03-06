use bytes::{BufMut, Bytes, BytesMut};
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(1_000);
    let mut acc = 0usize;

    for i in 0..iters {
        let mut buf = BytesMut::with_capacity(256);
        buf.put_u32((i as u32).wrapping_mul(17));
        buf.extend_from_slice(b"rusteze-bytes-bench");
        buf.extend_from_slice(&[0u8; 32]);
        let frozen: Bytes = buf.freeze();
        let mid = (frozen.len() / 2).max(1);
        let left = frozen.slice(..mid);
        let right = frozen.slice(mid..);
        acc ^= left.len() ^ right.len();
        acc ^= left.first().copied().unwrap_or(0) as usize;
    }

    black_box(acc);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{acc}");
}
