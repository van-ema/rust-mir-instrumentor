use std::hint::black_box;

// Inspired by Miri SB tests where a shared borrow stays live across a write.
//
// We create a shared reference into a `Box`, write through the box (unique),
// then read again through the shared ref.
fn main() {
    let b: Box<u8> = Box::new(0);
    let p: *mut u8 = Box::into_raw(b);

    unsafe {
        let r_unique: &mut u8 = &mut *p;
        let r_shared: &u8 = &*p;

        black_box(*r_shared);
        *r_unique = 1;
        // UB under SB-lite: read through shared after a conflicting write.
        black_box(*r_shared);
        drop(Box::from_raw(p));
    }
}
