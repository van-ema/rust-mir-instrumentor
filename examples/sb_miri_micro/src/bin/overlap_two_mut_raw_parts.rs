fn main() {
    let mut buf = [0u8; 8];
    let p = buf.as_mut_ptr();

    unsafe {
        // First mutable slice covers the full allocation.
        let a = core::slice::from_raw_parts_mut(p, 8);
        // Second mutable slice overlaps `a` on bytes [4, 8).
        let b = core::slice::from_raw_parts_mut(p.add(4), 4);

        // Keep both borrows live and observable.
        a[4] = 1;
        b[0] = 2;
        core::hint::black_box((a, b));
    }
}
