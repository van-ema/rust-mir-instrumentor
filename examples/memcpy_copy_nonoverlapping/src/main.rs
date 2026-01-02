fn main() {
    // Purpose: copy memory with ptr::copy_nonoverlapping.
    // Expected: READ on source pointer and WRITE on destination pointer.
    // Validates: memcpy-style intrinsic classification and non-deref effects.
    let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let mut dst = [0u8; 8];
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), src.len());
    }
    std::hint::black_box(&dst);
}
