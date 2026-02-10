fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let mut bytes = [0u8; 8];
    for (i, b) in data.iter().take(8).enumerate() {
        bytes[i] = *b;
    }

    let n = u64::from_le_bytes(bytes);
    let mut buf = itoa::Buffer::new();
    let s = buf.format(n);
    let _ = s.as_bytes();
}
