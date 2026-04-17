// Ported from miri/tests/fail/both_borrows/illegal_write1.rs.
#![allow(invalid_reference_casting)]

fn main() {
    let target = Box::new(42);
    let xref = &*target;
    {
        let x: *mut u32 = xref as *const _ as *mut _;
        unsafe { *x = 42 };
    }
    let _x = *xref;
}
