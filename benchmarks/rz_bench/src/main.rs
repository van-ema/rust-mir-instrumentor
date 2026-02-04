use bytes::{Buf, BufMut, Bytes, BytesMut};
use smallvec::SmallVec;
use std::hint::black_box;

fn run_bytes(iters: u64) {
    for _ in 0..iters {
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
    }
}

fn run_smallvec(iters: u64) {
    for _ in 0..iters {
        let mut v: SmallVec<u8, 8> = SmallVec::new();
        for i in 0..16u8 {
            v.push(i);
        }

        let sum: u32 = v.iter().map(|&x| x as u32).sum();
        v.remove(0);
        v.insert(1, 42);

        let slice = &v[..];
        let mut v2: SmallVec<u8, 4> = SmallVec::from_slice_copy(slice);
        v2.extend_from_slice(&[1, 2, 3, 4]);

        black_box(sum + v2.len() as u32);
    }
}

fn main() {
    let mut iters: u64 = 1000;
    let mut which = "both".to_string(); // bytes | smallvec | both

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--iters" => {
                let v = args.next().expect("--iters requires a value");
                iters = v.parse().expect("invalid --iters value");
            }
            "--which" => {
                which = args.next().expect("--which requires a value");
            }
            _ => {}
        }
    }

    match which.as_str() {
        "bytes" => run_bytes(iters),
        "smallvec" => run_smallvec(iters),
        "both" => {
            run_bytes(iters);
            run_smallvec(iters);
        }
        _ => {
            eprintln!("unknown --which={which} (expected bytes|smallvec|both)");
            std::process::exit(2);
        }
    }
}

