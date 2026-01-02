fn main() {
    // Purpose: initialize memory with ptr::write_bytes.
    // Expected: WRITE on the destination pointer.
    // Validates: memset-style intrinsic classification and size handling.
    let mut buf = [0u8; 16];
    unsafe {
        std::ptr::write_bytes(buf.as_mut_ptr(), 0xA5, buf.len());
    }
    std::hint::black_box(&buf);
}
