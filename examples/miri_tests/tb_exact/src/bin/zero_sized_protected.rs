// Ported from miri/tests/fail/both_borrows/zero-sized-protected.rs.
//@compile-flags: -Zmiri-tree-borrows

use std::alloc::{alloc, dealloc, Layout};

fn test(_x: &mut (), ptr: *mut u8, l: Layout) {
    unsafe { dealloc(ptr, l) };
}

fn main() {
    let l = Layout::from_size_align(1, 1).unwrap();
    let ptr = unsafe { alloc(l) };
    unsafe { test(&mut *ptr.cast::<()>(), ptr, l) };
}
