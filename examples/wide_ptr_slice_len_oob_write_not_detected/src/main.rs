use std::hint::black_box;

fn main() {
    let mut v = vec![0u8; 16];
    let p = v.as_mut_ptr();

    // Make a *short* slice (len = 1) backed by a larger allocation (len = 16).
    let fat: *mut [u8] = std::ptr::slice_from_raw_parts_mut(p, 1);

    unsafe {
        // OOB with respect to the *slice length* (index 7 >= 1),
        // but still in-bounds for the allocation.
        //
        // This panics in Rust before the runtime hook because slice indexing
        // performs an explicit bounds check on length metadata.
        (*fat)[7] = 42;
        black_box(*p);
    }
}
