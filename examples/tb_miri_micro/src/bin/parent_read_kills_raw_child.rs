// Inspired by miri/tests/fail/tree_borrows/parent_read_freezes_raw_mut.rs.
// Reading through the ancestor unique reference does not act as a foreign
// access on its raw child. A direct access through the root place is different
// and is covered by `miri_tb_exact::parent_read_freezes_raw_mut`.
fn main() {
    let mut root = 6u8;
    let mref = &mut root;
    let ptr = mref as *mut u8;

    unsafe {
        *ptr = 0;
        let _observe_parent = *mref;
        *ptr = 1;
    }
}
