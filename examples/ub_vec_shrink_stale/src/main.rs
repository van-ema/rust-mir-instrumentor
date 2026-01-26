fn main() {
    let mut v = vec![0u8; 64];
    let p = v.as_mut_ptr();
    // force shrink (may or may not move; either way size changes)
    v.truncate(8);
    v.shrink_to_fit();

    unsafe {
        // Old pointer p may now be stale or point into smaller allocation.
        *p.add(63) = 1;
    }
}
