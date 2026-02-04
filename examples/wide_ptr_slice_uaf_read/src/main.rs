use std::hint::black_box;

fn main() {
    let v = vec![0u8; 8];
    let p = v.as_ptr();
    let len = v.len();

    // Construct a wide raw pointer without casting to a thin pointer.
    let fat: *const [u8] = std::ptr::slice_from_raw_parts(p, len);

    // Free the allocation.
    drop(v);

    unsafe {
        // Use-after-free through a wide pointer.
        black_box((*fat)[0]);
    }
}

