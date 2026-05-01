// Ported from miri/tests/fail/both_borrows/return_invalid_shr_option.rs.
// Intent: returning invalidated shared ref inside Option should be rejected.
//
// Current rusteze status: rejected under tree-borrows checks.
fn foo(x: &mut (i32, i32)) -> Option<&i32> {
    let xraw = x as *mut (i32, i32);
    let ret = Some(unsafe { &(*xraw).1 });
    unsafe { *xraw = (42, 23) };
    ret
}

fn main() {
    match foo(&mut (1, 2)) {
        Some(_x) => {}
        None => {}
    }
}
