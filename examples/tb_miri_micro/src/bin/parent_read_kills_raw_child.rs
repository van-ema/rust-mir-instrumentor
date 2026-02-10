// Inspired by miri/tests/fail/tree_borrows/parent_read_freezes_raw_mut.rs.
// Tree Borrows rule: reading through the parent unique reference invalidates
// descendant mutable/raw children for future writes.
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
