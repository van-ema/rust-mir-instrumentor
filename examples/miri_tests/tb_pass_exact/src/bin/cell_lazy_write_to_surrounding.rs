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
    // Principled TB-lite now rejects this: `&pair.0` does not carry any
    // writable surrounding extent for `pair.1`, so the raw write remains
    // frozen outside the interior-mutable root.
    foo(&pair.0);
}
