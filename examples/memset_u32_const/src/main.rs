use std::ptr;

pub fn test_memset_u32_const() {
    let mut buf = [0u32; 8];

    unsafe {
        ptr::write_bytes(buf.as_mut_ptr(), 0u8, 8);
    }
}

fn main() {
    test_memset_u32_const();
}
