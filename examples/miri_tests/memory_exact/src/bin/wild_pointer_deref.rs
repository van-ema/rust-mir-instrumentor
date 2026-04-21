// Ported from miri/tests/fail/dangling_pointers/wild_pointer_deref.rs.
//@compile-flags: -Zmiri-permissive-provenance

fn main() {
    let p = 44 as *const i32;
    let x = unsafe { *p };
    panic!("this should never print: {}", x);
}
