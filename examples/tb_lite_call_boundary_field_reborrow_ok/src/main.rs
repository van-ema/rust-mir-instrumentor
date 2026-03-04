use std::hint::black_box;

struct Buffers {
    structural_indexes: Vec<u32>,
}

#[inline(never)]
fn consume_indexes(indexes: &mut Vec<u32>) -> usize {
    black_box(indexes.len());
    indexes.push(7);
    indexes.pop();
    black_box(indexes.len())
}

#[inline(never)]
fn parse_like(buf: &mut Buffers) -> usize {
    let root = &mut *buf;
    let mut acc = 0usize;

    // Repeated call-boundary field reborrows, followed by a fresh field-side read in the caller.
    // Each helper borrow ends at the call, so the final read is safe Rust and should not trip
    // TB-lite invalidation.
    for _ in 0..8 {
        acc ^= consume_indexes(&mut root.structural_indexes);
    }

    acc ^ black_box(root.structural_indexes.len())
}

fn main() {
    let mut buffers = Buffers {
        structural_indexes: vec![1, 2, 3, 4],
    };
    let out = parse_like(&mut buffers);
    black_box(out);
}
