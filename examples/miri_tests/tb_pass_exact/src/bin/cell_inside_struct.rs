// Ported from miri/tests/pass/tree_borrows/cell-inside-struct.rs.
//! Miri has a permissive variant of this test with precise interior-mutability
//! tracking disabled. Rusteze now always uses the precise interior-mut extent
//! model, so the write that escapes the `Cell` field's explicit extent remains
//! a TB-lite frozen-write violation here.
//@compile-flags: -Zmiri-tree-borrows -Zmiri-tree-borrows-no-precise-interior-mut
#[path = "../../utils/mod.rs"]
#[macro_use]
mod utils;

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
        let a = &root;

        name!(a as *const Foo, "a");

        let a: *const Foo = a as *const Foo;
        let a: *mut Foo = a as *mut Foo;

        let alloc_id = alloc_id!(a);
        print_state!(alloc_id);

        // Writing to `field2`, which is interior mutable, should be allowed.
        (*a).field2.set(10);

        // Under rusteze's always-precise interior-mut model, this write is
        // outside the `Cell` field's explicit writable extent and should fail.
        (*a).field1 = 88;
    }
}
