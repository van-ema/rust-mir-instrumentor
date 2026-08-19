// Ported from miri/tests/fail/both_borrows/return_invalid_shr.rs.
// Intent: returning an invalidated shared ref should violate the alias model.
//
// Current rusteze status: this is still `ok` (known gap).
// Desired behavior: this should be caught at return-boundary retag time; at minimum,
// it should be caught once the returned reference is actually dereferenced by the caller.
fn foo(x: &mut (i32, i32)) -> &i32 {
    let xraw = x as *mut (i32, i32);
    let ret = unsafe { &(*xraw).1 };
    unsafe { *xraw = (42, 23) };
    ret
}

fn main() {
    let _ = foo(&mut (1, 2));
}
