// Ported from miri/tests/fail/provenance/ptr_int_unexposed.rs.
//@compile-flags: -Zmiri-permissive-provenance

fn main() {
    let x: i32 = 3;
    let x_ptr = &x as *const i32;

    let x_usize: usize = x_ptr.addr();
    let ptr = std::ptr::with_exposed_provenance::<i32>(x_usize);
    assert_eq!(unsafe { *ptr }, 3);
}
