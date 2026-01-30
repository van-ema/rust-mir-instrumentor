use std::hint::black_box;

// Inspired by Miri SB tests that trigger violations across a call boundary.
fn do_write_raw(p: *mut u8) {
    unsafe {
        *p = 1;
    }
}

fn main() {
    let mut x = 0u8;

    // Create a raw pointer derived from the first unique borrow.
    let raw: *mut u8 = {
        let r1: &mut u8 = &mut x;
        &raw mut *r1
    };

    // Create a newer unique borrow, then use the older-derived raw pointer across a call.
    let r2: &mut u8 = &mut x;
    black_box(r2);
    do_write_raw(raw); // UB under SB-lite: raw write after newer unique
}
