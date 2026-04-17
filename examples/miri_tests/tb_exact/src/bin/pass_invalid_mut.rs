// Ported from miri/tests/fail/tree_borrows/pass_invalid_mut.rs.
fn foo(nope: &mut i32) {
    *nope = 31;
}

fn main() {
    let x = &mut 42;
    let xraw = x as *mut _;
    let xref = unsafe { &mut *xraw };
    *xref = 18;
    let _val = unsafe { *xraw };
    foo(xref);
}
