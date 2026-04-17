// Ported from miri/tests/fail/both_borrows/aliasing_mut1.rs.
use std::mem;

fn safe(x: &mut i32, y: &mut i32) {
    *x = 1;
    *y = 2;
}

fn main() {
    let mut x = 0;
    let xraw: *mut i32 = unsafe { mem::transmute(&mut x) };
    let safe_raw: fn(x: *mut i32, y: *mut i32) =
        unsafe { mem::transmute::<fn(&mut i32, &mut i32), _>(safe) };
    safe_raw(xraw, xraw);
}
