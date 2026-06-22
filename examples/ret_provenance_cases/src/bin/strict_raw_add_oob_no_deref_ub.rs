fn main() {
    let mut buf = [0u8; 4];
    let base = buf.as_mut_ptr();

    unsafe {
        let out = base.add(buf.len() + 4);
        std::hint::black_box(out);
    }
}
