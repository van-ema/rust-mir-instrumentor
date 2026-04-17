// Ported from miri/tests/fail/both_borrows/pass_invalid_shr.rs.
// Intent: passing invalidated shared ref should be rejected.
fn foo(_: &i32) {}

fn main() {
    let x = &mut 42;
    let xraw = x as *mut _;
    let xref = unsafe { &*xraw };
    unsafe { *xraw = 42 };
    foo(xref);
}
