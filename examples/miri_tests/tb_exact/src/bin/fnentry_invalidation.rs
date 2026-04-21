// Ported from miri/tests/fail/tree_borrows/fnentry_invalidation.rs.
//@compile-flags: -Zmiri-tree-borrows

fn main() {
    let mut x = 0i32;
    let z = &mut x as *mut i32;
    unsafe {
        *z = 1;
    }
    x.do_bad();
    unsafe {
        *z = 2;
    }
}

trait Bad {
    fn do_bad(&mut self) {}
}

impl Bad for i32 {}
