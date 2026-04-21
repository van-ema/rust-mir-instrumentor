// Ported from miri/tests/fail/tree_borrows/cell-inside-struct.rs.
//@compile-flags: -Zmiri-tree-borrows

use std::cell::Cell;

struct Foo {
    field1: u32,
    field2: Cell<u32>,
}

pub fn main() {
    let root = Foo {
        field1: 42,
        field2: Cell::new(88),
    };
    unsafe {
        let a = (&root as *const Foo).cast_mut();
        (*a).field2.set(10);
        (*a).field1 = 88;
    }
}
