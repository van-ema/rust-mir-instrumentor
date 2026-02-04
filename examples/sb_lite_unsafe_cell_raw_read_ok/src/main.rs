use std::cell::UnsafeCell;
use std::hint::black_box;

fn main() {
    let cell = UnsafeCell::new(0u8);

    // Derive a raw pointer from a shared reference to UnsafeCell.
    let raw = {
        let r_shared: &UnsafeCell<u8> = &cell;
        r_shared.get()
    };

    // Create a unique reference to the same location (interior mutability).
    let r_unique = unsafe { &mut *cell.get() };
    black_box(r_unique);

    // This read should be permitted under UnsafeCell's aliasing rules.
    unsafe {
        let v = *raw;
        black_box(v);
    }
}
