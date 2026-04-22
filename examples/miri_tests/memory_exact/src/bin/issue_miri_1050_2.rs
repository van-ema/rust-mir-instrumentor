// Ported from miri/tests/fail/both_borrows/issue-miri-1050-2.rs.
use std::ptr::NonNull;

fn main() {
    unsafe {
        let ptr = NonNull::<i32>::dangling();
        drop(Box::from_raw(ptr.as_ptr()));
    }
}
