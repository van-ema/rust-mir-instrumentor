// Ported from miri/tests/fail/tree_borrows/alternate-read-write.rs.
fn main() {
    let x = &mut 0u8;
    let y = unsafe { &mut *(x as *mut u8) };
    let _val = *x;
    *y += 1;
    let _val = *x;
    *y += 1;
    let _val = *x;
    *y += 1;
}
