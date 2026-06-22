fn main() {
    let mut buf = [0u8; 8];
    let base = buf.as_mut_ptr();

    unsafe {
        let metadata = base.wrapping_add(buf.len() + 8);
        std::hint::black_box(metadata);
        *base = 1;
    }

    std::hint::black_box(buf);
}
