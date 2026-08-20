use std::hint::black_box;

// Inspired by Miri SB tests around reborrowing:
// taking a raw pointer from an existing unique ref and then creating a new
// unique ref to the same location.
fn main() {
    let mut x = 0u8;

    let mut_ref: &mut u8 = &mut x;
    let p: *mut u8 = mut_ref as *mut u8;

    unsafe {
        let r2: &mut u8 = &mut *p;
        *r2 = 1;
        *mut_ref = 2; // Older unique used after a newer unique.
    }

    black_box(x);
}
