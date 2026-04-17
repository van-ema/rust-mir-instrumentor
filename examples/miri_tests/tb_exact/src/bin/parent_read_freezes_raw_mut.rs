// Ported from miri/tests/fail/tree_borrows/parent_read_freezes_raw_mut.rs.
fn main() {
    unsafe {
        let mut root = 6u8;
        let mref = &mut root;
        let ptr = mref as *mut u8;
        *ptr = 0;
        assert_eq!(root, 0);
        *ptr = 0;
    }
}
