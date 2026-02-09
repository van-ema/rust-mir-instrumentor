// Ported from Miri: tests/fail/both_borrows/buggy_as_mut_slice.rs
//
// The helper unsafely creates `&mut [T]` from `&Vec<T>`, so two mutable slices
// alias the same backing storage. Index writes through both should violate SB-lite.
mod safe {
    use std::slice::from_raw_parts_mut;

    pub fn as_mut_slice<T>(self_: &Vec<T>) -> &mut [T] {
        unsafe { from_raw_parts_mut(self_.as_ptr() as *mut T, self_.len()) }
    }
}

fn main() {
    let v = vec![0_i32, 1, 2];
    let v1 = safe::as_mut_slice(&v);
    let v2 = safe::as_mut_slice(&v);
    v1[1] = 5;
    v2[1] = 7;
}

