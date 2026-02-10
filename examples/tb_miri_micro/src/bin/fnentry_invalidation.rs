// Ported from miri/tests/fail/tree_borrows/fnentry_invalidation.rs.
trait Bad {
    fn do_bad(&mut self) {
        // no-op
    }
}

impl Bad for i32 {}

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
