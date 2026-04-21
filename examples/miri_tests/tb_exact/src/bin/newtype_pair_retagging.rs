// Ported from miri/tests/fail/both_borrows/newtype_pair_retagging.rs.
//@compile-flags: -Zmiri-tree-borrows

struct Newtype<'a>(#[allow(dead_code)] &'a mut i32, #[allow(dead_code)] i32);

fn dealloc_while_running(_n: Newtype<'_>, dealloc: impl FnOnce()) {
    dealloc();
}

fn main() {
    let ptr = Box::into_raw(Box::new(0i32));
    unsafe {
        dealloc_while_running(Newtype(&mut *ptr, 0), || drop(Box::from_raw(ptr)));
    };
}
