use std::ptr;

fn main() {
    // Zero-sized copy should be a no-op even with dangling pointers.
    let src = ptr::NonNull::<u8>::dangling().as_ptr();
    let dst = ptr::NonNull::<u8>::dangling().as_ptr() as *mut u8;

    unsafe {
        ptr::copy_nonoverlapping(src, dst, 0);
    }
}
