// Ported from miri/tests/fail/stacked_borrows/return_invalid_mut.rs.
// Returning an invalidated `&mut` should be rejected.
fn foo(x: &mut (i32, i32)) -> &mut i32 {
    let xraw = x as *mut (i32, i32);
    let ret = unsafe { &mut (*xraw).1 };
    let _val = unsafe { *xraw }; // invalidates `ret`
    ret
}

fn main() {
    let _ = foo(&mut (1, 2));
}
