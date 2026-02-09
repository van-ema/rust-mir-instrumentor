fn main() {
    let mut v = vec![0u8; 64];
    let p = v.as_mut_ptr();
    // Build a new short Vec and drop the old allocation to make `p` stale deterministically.
    let short = v[..8].to_vec();
    drop(v);

    unsafe {
        // Old pointer is stale after dropping the original Vec allocation.
        *p.add(63) = 1;
    }

    std::hint::black_box(short);
}
