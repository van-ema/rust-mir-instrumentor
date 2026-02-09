use std::hint::black_box;

struct Cursor {
    index: usize,
}

#[inline(never)]
fn parse_like(c: &mut Cursor) -> usize {
    let mut start: usize = c.index;

    // This creates a temporary unique reborrow of the `index` field.
    let idx_ref: &mut usize = &mut c.index;
    *idx_ref += 1;
    black_box(*idx_ref);

    // Safe Rust: once `idx_ref` is no longer used, reading through `c` is valid.
    // SB-lite currently reports a violation here in some paths.
    start ^= c.index;
    start
}

fn main() {
    let mut c = Cursor { index: 7 };
    let out = parse_like(&mut c);
    black_box(out);
}
