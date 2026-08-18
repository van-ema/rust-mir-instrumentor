// Ported from miri/tests/pass/tree_borrows/cell-inside-struct.rs.
//! The same as Miri's fail test, but with precise interior-mutability tracking
//! explicitly disabled. Rusteze honors that opt-in revision without changing
//! the sound default model.
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

        // With precise tracking disabled, the surrounding allocation carries
        // the Cell permission and this write is allowed.
        (*a).field1 = 88;
    }
}
