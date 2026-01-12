use std::ptr;

pub fn test_copy_u32_dynamic(n: usize) {
    let src = vec![1u32; n];
    let mut dst = vec![0u32; n];

    unsafe {
        ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), n);
    }

    std::hint::black_box(&dst);
}

fn main() {
    let n = std::env::args()
        .nth(1)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(16);
    test_copy_u32_dynamic(n);
}
