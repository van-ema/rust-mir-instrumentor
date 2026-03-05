pub fn parse_iters(default_iters: u64) -> u64 {
    if let Ok(v) = std::env::var("ECO_BENCH_ITERS") {
        return v.parse().expect("invalid ECO_BENCH_ITERS value");
    }

    let mut iters = default_iters;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--iters" {
            let v = args.next().expect("--iters requires a value");
            iters = v.parse().expect("invalid --iters value");
        }
    }
    iters
}
