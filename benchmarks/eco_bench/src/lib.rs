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

pub fn maybe_dump_rusteze_hook_profile() {
    #[cfg(not(feature = "rusteze-profile"))]
    {
        return;
    }

    #[cfg(feature = "rusteze-profile")]
    unsafe {
        unsafe extern "C" {
            fn __rz_dump_hook_profile();
        }

        // Only enabled for dedicated profiling runs; normal benchmarks stay clean.
        let enabled = std::env::var("RZ_PROFILE_HOOKS")
            .ok()
            .map_or(false, |v| v != "0" && !v.eq_ignore_ascii_case("false"));
        if enabled {
            __rz_dump_hook_profile();
        }
    }
}
