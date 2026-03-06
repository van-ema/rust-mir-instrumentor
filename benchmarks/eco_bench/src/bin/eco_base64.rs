use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(200);
    let input = b"rusteze eco bench payload 0123456789";
    let mut total = 0usize;

    for _ in 0..iters {
        let encoded = STANDARD.encode(input);
        let decoded = STANDARD.decode(encoded.as_bytes()).expect("decode failed");
        total ^= decoded.len();
    }

    black_box(total);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{total}");
}
