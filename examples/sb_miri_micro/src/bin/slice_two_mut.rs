use std::hint::black_box;

// Inspired by Miri SB tests like "buggy_split_at_mut": overlapping mutable
// views created from the same base pointer.
//
// NOTE: We intentionally use a fixed-size array reference (`&mut [u8; 2]`)
// instead of `&mut [u8]` (a wide pointer). Our instrumentation currently only
// tracks thin pointers, and wide-pointer modeling is a separate TODO.
fn main() {
    let mut a = [0u8, 1u8, 2u8, 3u8];
    let p = a.as_mut_ptr();

    unsafe {
        let p2: *mut [u8; 2] = p as *mut [u8; 2];
        let s1: &mut [u8; 2] = &mut *p2;
        let s2: &mut [u8; 2] = &mut *p2;

        s2[0] = 10;
        s1[0] = 11; // UB: older unique used after a newer unique
    }

    black_box(a);
}
