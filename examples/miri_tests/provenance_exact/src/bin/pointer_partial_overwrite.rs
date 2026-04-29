// Ported from miri/tests/fail/provenance/pointer_partial_overwrite.rs.
//@compile-flags: -Zmiri-disable-alignment-check -Zmiri-disable-stacked-borrows -Zmiri-disable-validation

fn main() {
    let mut p = &42;
    unsafe {
        let ptr: *mut _ = &mut p;
        *(ptr as *mut u8) = 123;
    }
    let x = *p;
    panic!("this should never print: {}", x);
}
