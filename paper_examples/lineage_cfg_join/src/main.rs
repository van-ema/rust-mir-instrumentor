// Purpose: demonstrate that SSA anchors survive a CFG join when the first use of
// the shared pointer expression occurs before the branch and the second use
// occurs after the join.
//
// Without join-aware anchor propagation, the block with two predecessors drops
// the cached ancestor expression and rebuilds the second `&mut` from a fresh
// root-like lineage. With the meet-based propagation, the join keeps the anchor
// because both predecessors carry the same entry, and the later read through
// `r_ref` reports a Tree Borrows violation.

#[inline(never)]
fn write_through(x: &mut u8) {
    unsafe { std::ptr::write_volatile(x, 1) }
}

#[inline(never)]
fn read_through(x: &mut u8) -> u8 {
    *x
}

fn main() {
    let mut v = vec![0u8; 16];
    let r_ref: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };

    if std::hint::black_box(true) {
        std::hint::black_box(0usize);
    } else {
        std::hint::black_box(1usize);
    }

    let s_ref: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };
    write_through(s_ref);
    let val = read_through(r_ref);
    println!("{val}");
}
