// Ported from Miri: tests/fail/both_borrows/buggy_split_at_mut.rs
//
// The buggy split computes the first slice length incorrectly, creating
// overlapping mutable slices. Indexed writes through both should violate Tree Borrows.
mod safe {
    use std::slice::from_raw_parts_mut;

    pub fn split_at_mut<T>(self_: &mut [T], mid: usize) -> (&mut [T], &mut [T]) {
        let len = self_.len();
        let ptr = self_.as_mut_ptr();

        unsafe {
            assert!(mid <= len);
            (
                from_raw_parts_mut(ptr, len - mid), // BUG: should be `mid`
                from_raw_parts_mut(ptr.add(mid), len - mid),
            )
        }
    }
}

fn main() {
    let mut array = [1_i32, 2, 3, 4];
    let (a, b) = safe::split_at_mut(&mut array, 0);
    a[1] = 5;
    b[1] = 6;
}
