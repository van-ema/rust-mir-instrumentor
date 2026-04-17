// Ported from miri/tests/fail/both_borrows/illegal_write5.rs.
fn main() {
    let mut x = 15;
    let xraw = &mut x as *mut _;
    let xref = unsafe { &mut *xraw };
    callee(xraw);
    let _val = *xref;
}

fn callee(xraw: *mut i32) {
    unsafe { *xraw = 15 };
}
