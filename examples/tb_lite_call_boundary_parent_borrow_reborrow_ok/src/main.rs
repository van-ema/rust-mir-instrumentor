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
    let indexes = &mut root.structural_indexes;
    let mut acc = 0usize;

    // Reduced simd-json shape:
    // one long-lived parent borrow, repeated call-boundary child reborrows derived from it,
    // then a final read through the original parent borrow.
    for _ in 0..8 {
        acc ^= consume_indexes(&mut *indexes);
    }

    acc ^ black_box(indexes.len())
}

fn main() {
    let mut buffers = Buffers {
        structural_indexes: vec![1, 2, 3, 4],
    };
    let out = parse_like(&mut buffers);
    black_box(out);
}
