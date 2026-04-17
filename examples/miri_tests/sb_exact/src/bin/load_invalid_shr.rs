// Ported from miri/tests/fail/both_borrows/load_invalid_shr.rs.
// Intent: loading invalidated shared ref from memory should be rejected.
//
// Current rusteze status: still `ok` (known gap).
fn main() {
    let x = &mut 42;
    let xraw = x as *mut _;
    let xref = unsafe { &*xraw };
    let xref_in_mem = Box::new(xref);
    unsafe { *xraw = 42 };
    let _val = *xref_in_mem;
}
