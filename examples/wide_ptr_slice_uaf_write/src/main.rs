use std::hint::black_box;

fn main() {
    let mut v = vec![0u8; 8];
    let p = v.as_mut_ptr();
    let len = v.len();

    // Construct a wide raw pointer without casting to a thin pointer.
    let fat: *mut [u8] = std::ptr::slice_from_raw_parts_mut(p, len);

    // Free the allocation.
    drop(v);

    unsafe {
        // Use-after-free through a wide pointer. This exercises data-pointer extraction for
        // fat pointers in the instrumentation pass.
        (*fat)[0] = 1;
    }

    black_box(fat as *mut u8);
}

