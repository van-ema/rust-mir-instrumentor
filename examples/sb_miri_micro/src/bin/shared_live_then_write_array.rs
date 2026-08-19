use std::hint::black_box;

// Inspired by Miri SB tests: keep a shared ref live across a write, then read
// through it again.
fn main() {
    let mut a = [0u8, 1u8];
    let p = (&mut a[0]) as *mut u8;

    unsafe {
        let r_unique: &mut u8 = &mut *p;
        let r_shared: &u8 = &*p;

        black_box(*r_shared);
        *r_unique = 3;

        // UB: shared borrow read after write while still live.
        black_box(*r_shared);
    }
}
