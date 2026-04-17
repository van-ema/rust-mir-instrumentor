// Ported from miri/tests/fail/both_borrows/pass_invalid_shr_option.rs.
// Intent: passing invalidated shared ref inside Option should be rejected.
fn foo(_: Option<&i32>) {}

fn main() {
    let x = &mut 42;
    let xraw = x as *mut _;
    let some_xref = unsafe { Some(&*xraw) };
    unsafe { *xraw = 42 };
    foo(some_xref);
}
