// Case: callee returns a pointer near the end, caller derives and accesses out of bounds.
// Goal: stress return-based raw derivation precision (OOB vs degraded classification).

#[inline(never)]
unsafe fn tail_ptr(p: *mut u8) -> *mut u8 {
    p.add(3)
}

fn main() {
    let mut buf = [0u8; 4];
    let base = buf.as_mut_ptr();
    let tail = unsafe { tail_ptr(base) };

    unsafe {
        *tail.add(2) = 1;
    }

    std::hint::black_box(buf);
}
