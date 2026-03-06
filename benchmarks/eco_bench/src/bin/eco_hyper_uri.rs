use hyper::Uri;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(1_200);
    let mut acc = 0usize;

    for i in 0..iters {
        let s = format!(
            "http://example.com/api/v1/items/{}?q=rusteze&n={}",
            i % 97,
            i % 13
        );
        let uri: Uri = s.parse().expect("uri parse failed");
        acc ^= uri.path().len();
        if let Some(q) = uri.query() {
            acc ^= q.len();
        }
    }

    black_box(acc);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{acc}");
}
