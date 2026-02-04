use std::hint::black_box;

fn main() {
    let s: &str = "hello";

    // Wide raw pointer: *const str (fat pointer).
    let fat: *const str = &raw const *s;

    // Drop metadata and read from the data pointer.
    let thin: *const u8 = fat as *const u8;
    unsafe {
        black_box(*thin);
    }
}

