// Ported from miri/tests/pass/tree_borrows/cell-lazy-write-to-surrounding.rs.
//@compile-flags: -Zmiri-tree-borrows

use std::cell::Cell;

fn foo(x: &Cell<i32>) {
    unsafe {
        let ptr = x as *const Cell<i32> as *mut Cell<i32> as *mut i32;
        ptr.offset(1).write(0);
    }
}

fn main() {
    let arr = [Cell::new(1), Cell::new(1)];
    foo(&arr[0]);

    let pair = (Cell::new(1), 1);
    // The shared reference to the interior-mutable field carries permission
    // for the surrounding aggregate, matching Tree Borrows.
    foo(&pair.0);
}
