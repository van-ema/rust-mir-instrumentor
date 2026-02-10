// Ported from miri/tests/fail/stacked_borrows/invalidate_against_protector1.rs.
// Using `x` while `y` is assumed unique for the duration of `inner` should be rejected.
fn inner(x: *mut i32, _y: &mut i32) {
    let _val = unsafe { *x };
}

fn main() {
    let mut x = 0;
    let xraw = &mut x as *mut _;
    let xref = unsafe { &mut *xraw };
    inner(xraw, xref);
}
