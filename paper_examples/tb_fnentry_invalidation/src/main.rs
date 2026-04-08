// Purpose: import Miri's Tree Borrows fn-entry invalidation case as a small
// standalone regression for rusteze.
//
// Adapted from Miri:
// `tests/fail/tree_borrows/fnentry_invalidation.rs`
//
// A raw pointer derived from `&mut x` is used after a later `&mut self` call
// on the same value. Under Tree Borrows, the function entry retag for `do_bad`
// invalidates the older raw access path.
//
// Expected in --release: ok
//
// Current classification: known gap. Miri rejects this at the function-entry
// retag boundary, but rusteze currently stays silent on this shape.

trait Bad {
    fn do_bad(&mut self) {}
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
