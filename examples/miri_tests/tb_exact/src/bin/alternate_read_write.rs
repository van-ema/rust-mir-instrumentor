// Ported from miri/tests/fail/tree_borrows/alternate-read-write.rs.
//@compile-flags: -Zmiri-tree-borrows

// Check that TB properly rejects alternating Reads and Writes, but tolerates
// alternating only Reads to Reserved mutable references.
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
