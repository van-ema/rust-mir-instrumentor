use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(1_500);
    let mut total = 0usize;

    for i in 0..iters {
        let mut buf = itoa::Buffer::new();
        let s = buf.format((i as i64).wrapping_mul(1_000_003).wrapping_add(17));
        total ^= s.len();
    }

    black_box(total);
    println!("{total}");
}
