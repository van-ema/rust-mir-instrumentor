fn main() {
    let mut x = 0i32;
    let p = &mut x as *mut i32;

    unsafe {
        // Create overlapping shared and unique references via a raw pointer.
        let r_shared: &i32 = &*p;
        let r_unique: &mut i32 = &mut *p;

        // Under SB-lite, the unique borrow is on top, so reading via the
        // shared borrow should report a STACKED_BORROWS_VIOLATION when enabled.
        std::hint::black_box(*r_shared);
        *r_unique = 1;
    }
}
