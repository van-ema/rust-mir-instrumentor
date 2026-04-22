// Ported from miri/tests/fail/dangling_pointers/out_of_bounds_read_neg_offset.rs.

fn main() {
    let v: Vec<u16> = vec![1, 2];
    let x = unsafe { *v.as_ptr().wrapping_byte_sub(5) };
    panic!("this should never print: {}", x);
}
