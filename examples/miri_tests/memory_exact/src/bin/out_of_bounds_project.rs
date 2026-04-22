// Ported from miri/tests/fail/dangling_pointers/out_of_bounds_project.rs.
//@compile-flags: -Zmiri-disable-alignment-check -Zmiri-disable-stacked-borrows -Zmiri-disable-validation

use std::ptr::addr_of;

fn main() {
    let v = 0u32;
    let ptr = addr_of!(v).cast::<(u32, u32, u32)>();
    unsafe {
        let _field = addr_of!((*ptr).1);
        let _field = addr_of!((*ptr).2);
    }
}
