// Inspired by miri/tests/fail/tree_borrows/reservedim_spurious_write.rs.
// Lightweight single-thread approximation focused on Reserved(IM)-like aliasing.
use std::cell::Cell;

fn main() {
    let mut data: u8 = 0;
    let x: &mut u8 = &mut data;
    let y: &mut Cell<u8> = unsafe { &mut *(x as *mut u8 as *mut Cell<u8>) };

    // Activate `x`.
    *x = 42;

    // Write via interior-mutable alias.
    let yp = y as *mut Cell<u8> as *mut u8;
    unsafe {
        *yp = 13;
    }

    let _ = y.get();
}
