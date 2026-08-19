#![allow(invalid_reference_casting)]

use std::hint::black_box;

// Ported (simplified) from Miri `fail/both_borrows/illegal_write1.rs`.
//
// Create a shared reference, cast it to `*mut`, and then write through the raw
// pointer. This should be rejected because the tag grants only
// shared permissions.
fn main() {
    let b: Box<u32> = Box::new(0);
    let xref: &u32 = &*b;

    let p: *mut u32 = xref as *const u32 as *mut u32;
    unsafe {
        *p = 42; // UB: write through raw derived from shared
    }

    black_box(*xref);
}
