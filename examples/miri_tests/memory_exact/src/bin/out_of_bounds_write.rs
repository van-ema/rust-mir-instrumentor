// Ported from miri/tests/fail/dangling_pointers/out_of_bounds_write.rs.
fn main() {
    let mut v: Vec<u16> = vec![1, 2];
    // This write is also misaligned. We make sure that the OOB message has priority.
    unsafe { *v.as_mut_ptr().wrapping_byte_add(5) = 0 };
}
