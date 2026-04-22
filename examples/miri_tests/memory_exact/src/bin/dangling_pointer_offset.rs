// Ported from miri/tests/fail/dangling_pointers/dangling_pointer_offset.rs.
//@compile-flags: -Zmiri-disable-alignment-check -Zmiri-disable-stacked-borrows -Zmiri-disable-validation

fn main() {
    let p = {
        let b = Box::new(42);
        &*b as *const i32
    };
    let x = unsafe { p.offset(42) };
    let _ = x;
}
