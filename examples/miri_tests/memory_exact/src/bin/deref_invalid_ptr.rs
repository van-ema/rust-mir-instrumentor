// Ported from miri/tests/fail/dangling_pointers/deref-invalid-ptr.rs.
//@compile-flags: -Zmiri-disable-validation -Zmiri-permissive-provenance

fn main() {
    let x = 16usize as *const u32;
    let _y = unsafe { &*x as *const u32 };
}
