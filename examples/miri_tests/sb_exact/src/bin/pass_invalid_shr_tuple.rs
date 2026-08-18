// Ported from miri/tests/fail/both_borrows/pass_invalid_shr_tuple.rs.
// Intent: passing invalidated shared refs through a tuple should be rejected by
// both Stacked Borrows and Tree Borrows.
fn foo(_: (&i32, &i32)) {}

fn main() {
    let x = &mut (42i32, 31i32);
    let xraw0 = &mut x.0 as *mut _;
    let xraw1 = &mut x.1 as *mut _;
    let pair_xref = unsafe { (&*xraw0, &*xraw1) };
    unsafe { *xraw0 = 42 };
    foo(pair_xref);
}
