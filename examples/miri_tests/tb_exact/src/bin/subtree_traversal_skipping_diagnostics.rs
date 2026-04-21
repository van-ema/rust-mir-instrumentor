// Ported from miri/tests/fail/tree_borrows/subtree_traversal_skipping_diagnostics.rs.
//@compile-flags: -Zmiri-tree-borrows -Zmiri-provenance-gc=0

fn write_to_mut(m: &mut u8, other_ptr: *const u8) {
    unsafe {
        std::hint::black_box(*other_ptr);
    }
    *m = 42;
}

fn main() {
    let root = 42u8;
    unsafe {
        let intermediary = &root;
        let data = &mut *(core::ptr::addr_of!(*intermediary) as *mut u8);
        write_to_mut(data, core::ptr::addr_of!(root));
    }
}
