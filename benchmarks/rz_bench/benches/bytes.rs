use bytes::{Buf, BufMut, Bytes, BytesMut};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bytes_smoke(c: &mut Criterion) {
    c.bench_function("bytes/smoke", |b| {
        b.iter(|| {
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

            black_box(final_buf.len());
        })
    });
}

criterion_group!(benches, bytes_smoke);
criterion_main!(benches);

