use std::ptr;

pub fn test_copy_alias_violation() {
    // Purpose: overlapping copy_nonoverlapping to trigger aliasing violation.
    // Expected: UB, but we do not yet check cross-argument overlap for copy_nonoverlapping.
    let mut x = [1u8, 2, 3, 4];
    let p1 = x.as_mut_ptr();
    let p2 = unsafe { p1.add(1) };

    unsafe {
        // overlapping copy — UB
        ptr::copy_nonoverlapping(p1, p2, 3);
    }
}

fn main() {
    test_copy_alias_violation();
}
