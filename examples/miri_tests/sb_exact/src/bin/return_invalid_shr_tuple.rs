// Ported from miri/tests/fail/both_borrows/return_invalid_shr_tuple.rs.
// Intent: returning an invalidated shared ref inside a tuple should violate the alias model.
//
// Current rusteze status: caught at the return boundary once by-value carrier anchor export is
// enabled for tuple-wrapped shared refs.
fn foo(x: &mut (i32, i32)) -> (&i32,) {
    let xraw = x as *mut (i32, i32);
    let ret = (unsafe { &(*xraw).1 },);
    unsafe { *xraw = (42, 23) };
    ret
}

fn main() {
    let _ = foo(&mut (1, 2)).0;
}
