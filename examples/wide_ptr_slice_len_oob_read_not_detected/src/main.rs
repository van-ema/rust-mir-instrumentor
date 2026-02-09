use std::hint::black_box;

fn main() {
    let v = vec![0u8; 16];
    let p = v.as_ptr();

    // Make a *short* slice (len = 1) backed by a larger allocation (len = 16).
    let fat: *const [u8] = std::ptr::slice_from_raw_parts(p, 1);

    unsafe {
        // OOB with respect to the *slice length* (index 5 >= 1),
        // but still in-bounds for the allocation.
        //
        // This panics in Rust before the runtime hook because slice indexing
        // performs an explicit bounds check on length metadata.
        black_box((*fat)[5]);
    }
}
