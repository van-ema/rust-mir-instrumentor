use mail_internals::utils::vec_insert_bytes;

fn main() {
    let mut buf = Vec::with_capacity(4);
    buf.extend_from_slice(b"ABCD");

    let src = unsafe { std::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
    vec_insert_bytes(&mut buf, 2, src);

    println!("{}", String::from_utf8_lossy(&buf));
}
