use std::hint::black_box;

fn main() {
    let mut arr = [0u8; 8];
    let s: &mut [u8] = &mut arr;

    // Wide raw pointer: *mut [u8] (fat pointer).
    let fat: *mut [u8] = &raw mut *s;

    // Drop metadata and write through the data pointer.
    let thin: *mut u8 = fat as *mut u8;
    unsafe {
        *thin.add(7) = 1;
    }

    black_box(arr);
}

