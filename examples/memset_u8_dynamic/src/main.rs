use std::ptr;

pub fn test_memset_u8_dynamic(n: usize) {
    let mut buf = vec![0u8; n];

    unsafe {
        ptr::write_bytes(buf.as_mut_ptr(), 0xAA, n);
    }

    for b in buf {
        assert_eq!(b, 0xAA);
    }
}

fn main() {
    let n = std::env::args()
        .nth(1)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(16);
    test_memset_u8_dynamic(n);
}
