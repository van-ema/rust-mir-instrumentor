use std::hint::black_box;

#[derive(Default)]
struct State {
    index: usize,
    scratch: usize,
}

#[inline(never)]
fn parse_like(index: &mut usize, scratch: &mut usize) -> usize {
    // Different field lineage than `index`.
    *scratch = scratch.wrapping_add(1);

    // Safe read through `index` must not be blocked by the newer unique borrow
    // on `scratch` just because both fields share the same parent allocation.
    *index
}

fn main() {
    let mut s = State::default();
    s.index = 7;
    s.scratch = 10;

    let out = parse_like(&mut s.index, &mut s.scratch);
    black_box(out);
    black_box(s.scratch);
}
