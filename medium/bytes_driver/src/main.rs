use bytes::{Buf, BufMut, Bytes, BytesMut};

fn main() {
    let mut buf = BytesMut::with_capacity(64);
    buf.put_slice(b"hello");
    buf.put_u32(0xAABBCCDD);

    let frozen = buf.freeze();
    let mut view = frozen.clone();
    let _first = view.get_u8();
    let rest: Bytes = view.copy_to_bytes(view.remaining());

    let mut buf2 = BytesMut::from(&rest[..]);
    buf2.put_u16(0xBEEF);
    let final_buf = buf2.freeze();

    println!("{}", final_buf.len());
}
