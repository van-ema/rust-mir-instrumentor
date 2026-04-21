// Ported from miri/tests/fail/tree_borrows/repeated_foreign_read_lazy_conflicted.rs.
//@compile-flags: -Zmiri-tree-borrows

use std::ptr::addr_of_mut;

fn do_something(_: u8) {}

unsafe fn access_after_sub_1(x: &mut u8, orig_ptr: *mut u8) {
    do_something(*orig_ptr);
    *(x as *mut u8).byte_sub(1) = 42;
}

pub fn main() {
    unsafe {
        let mut alloc = [0u8, 0u8];
        let orig_ptr = addr_of_mut!(alloc) as *mut u8;
        let foo = &mut *orig_ptr;
        do_something(alloc[0]);
        access_after_sub_1(&mut *(foo as *mut u8).byte_add(1), orig_ptr);
    }
}
