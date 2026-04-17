// Ported from miri/tests/fail/both_borrows/aliasing_mut2.rs.
use std::mem;

fn safe(x: &i32, y: &mut i32) {
    let _v = *x;
    *y = 2;
}

fn main() {
    let mut x = 0;
    let xref = &mut x;
    let xraw: *mut i32 = unsafe { mem::transmute_copy(&xref) };
    let xshr = &*xref;
    let safe_raw: fn(x: *const i32, y: *mut i32) =
        unsafe { mem::transmute::<fn(&i32, &mut i32), _>(safe) };
    safe_raw(xshr, xraw);
}
