#[inline(never)]
fn sink_ptr(p: *const i32) {
    // Don't deref; we only care that passing `p` counts as a PtrUse and carries a tag.
    std::hint::black_box(p);
}

fn main() {
    let x = 123i32;
    let p: *const i32 = &x as *const i32;
    sink_ptr(p);
}
