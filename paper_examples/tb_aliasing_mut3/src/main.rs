// Purpose: import a Miri both-borrows case that rusteze's tb_lite model
// already catches.
//
// Adapted from Miri:
// `tests/fail/both_borrows/aliasing_mut3.rs`
//
// A raw mutable pointer is forged from `&mut xref`, then passed alongside a
// shared reference into a function entered through an unretagged function
// pointer. The write through the raw mutable path invalidates the shared
// reference, and the later read through that shared reference is UB.
//
// Expected in --release: TREE_BORROWS_VIOLATION|READ|RefShared|4

use std::mem;

#[inline(never)]
pub fn safe(x: &mut i32, y: &i32) {
    *x = 1;
    let v = *y;
    std::hint::black_box(v);
}

fn main() {
    let mut x = 0;
    let xref = &mut x;
    let xraw: *mut i32 = unsafe { mem::transmute_copy(&xref) };
    let xshr = &*xref;
    let safe_raw: fn(x: *mut i32, y: *const i32) =
        unsafe { mem::transmute::<fn(&mut i32, &i32), _>(safe) };
    safe_raw(xraw, xshr);
}
