// Case: callee returns a raw pointer derived from caller raw pointer.
// Goal: stress parent lineage propagation for returned raw pointers.

#[inline(never)]
unsafe fn bounce_raw(p: *mut u8) -> *mut u8 {
    p
}

fn main() {
    let mut x = 0u8;
    let base = &mut x as *mut u8;
    let ret = unsafe { bounce_raw(base) };

    unsafe {
        *ret = 1;
        *base = 2;
    }

    std::hint::black_box(x);
}
