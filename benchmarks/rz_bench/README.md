# rz_bench

Dedicated benchmarking crate for evaluating overhead on representative `bytes` and `smallvec`
workloads.

## Criterion (microbench)

```bash
cargo bench -p rz_bench --bench bytes
cargo bench -p rz_bench --bench smallvec
```

## Binary (for end-to-end timing / rusteze instrumentation)

This crate also ships a normal binary so it can be instrumented via `cargo instrument-mir`
and timed by `scripts/bench_overhead.py`.

```bash
# baseline build+run
cargo run -p rz_bench --release -- --iters 100000 --which bytes

# instrumented build
RZ_INSTRUMENT_ALL_DEPS=1 cargo instrument-mir --runtime-path=target/release -p rz_bench --bin rz_bench --release
./target/release/rz_bench --iters 100000 --which bytes
```

Note: `bytes` and `smallvec` are path dependencies under `third_party/`, so make sure those
repos are cloned (see `third_party/README.md`).

