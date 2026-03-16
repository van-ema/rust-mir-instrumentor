# rust-mir-instrumentor

## Build
```
cargo build --release -p runtime
cargo install --path instrument-mir --bin instrument-mir
cargo install --path instrument-mir --bin cargo-instrument-mir
```

## Use

```bash
cargo build -p runtime --release
cargo instrument-mir --runtime-path=target/release --mir-out=./out.mir -p hello --bin hello --release
./target/release/hello
```

You can specify output MIR files with `--mir-out` and provide a runtime path with `--runtime-path`.
Note: `cargo instrument-mir` only builds the binary; run the produced binary directly (do not use `cargo run`).
MIR output is only emitted during compilation. If Cargo says the target is up to date, no `before.*` or `after.*` file is written. Run `cargo clean -p <crate>` or touch a source file to force a rebuild. `--mir-out` accepts either a file path or a directory. If you pass a directory or a path ending with a separator, the tool writes `before.out.mir` and `after.out.mir` inside that directory. If you pass a file path, the tool writes `before.<name>` and `after.<name>` alongside that file. `~` is expanded and parent directories are created as needed. The resolved paths follow these rules and are not printed by default.

Run with tracing logs and caller locations:

```bash
RZ_LOG=Trace RZ_LOG_LOC=1 ./target/release/hello
```

```bash
cargo clean -p medium_bytes_driver
RZ_INSTRUMENT_ALL_DEPS=1 \
cargo instrument-mir \
  --runtime-path=target/debug \
  --mir-out=./out.medium_bytes_driver.mir \
  -p medium_bytes_driver --bin medium_bytes_driver

# Alternative: force a rebuild without cleaning
touch medium/bytes_driver/src/main.rs
```

## Makefile shortcuts

```bash
# Build + instrument an example (debug by default)
make instrument EXAMPLE=hello

# Run the instrumented example
make run EXAMPLE=hello

# Clean + rebuild + instrument
make rebuild EXAMPLE=hello

# Release profile
make instrument EXAMPLE=hello PROFILE=release
make run EXAMPLE=hello PROFILE=release
```

## Running with dataflow analysis

There are three distinct analysis modes.

### 1. Metadata-local dataflow only

This is the sound intra-procedural pass that prunes redundant metadata propagation
(`TagProp`, ref-ancestor propagation). It does not change semantic hooks.

```bash
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_METADATA_DATAFLOW=1 \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh
```

Add `RZ_METADATA_DATAFLOW_STATS=1` to print pruning totals during compilation.

### 2. Local backward unsafe-sensitive analysis

This is the active hook-gating analysis. It starts from unsafe-sensitive sinks in
each MIR body, propagates relevance backward, and prunes access/creation hooks for
pointer locals that cannot reach those sinks inside the current function.

```bash
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_UNSAFE_DATAFLOW=1 \
RZ_UNSAFE_DATAFLOW_STATS=1 \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh
```

This mode is single-build and does not require summary artifacts.

### 3. Interprocedural unsafe-sensitive analysis

This is the current cross-crate path. It runs in three phases:

1. analyze-only build dumping per-function unsafe summaries
2. offline fixed-point merge across crates
3. normal instrumented build consuming merged summaries conservatively

Use the wrapper:

```bash
AFL_PATH=/path/to/AFLplusplus \
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_INTERPROC_UNSAFE_SUMMARIES=1 \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh
```

The wrapper delegates to `scripts/afl_build_interproc.sh`.

Manual equivalent:

```bash
AFL_PATH=/path/to/AFLplusplus \
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
HARNESS_TARGET_DIR=./target/afl-release-bytes-summary \
RZ_ANALYZE_UNSAFE_SUMMARIES=1 \
RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP=1 \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh

python3 ./scripts/merge_unsafe_summaries.py \
  --input-dir ./target/afl-release-bytes-summary/rusteze-unsafe-summaries \
  --output-dir ./target/afl-release-bytes-merged \
  --report ./target/afl-release-bytes-merged/merge.report.txt

AFL_PATH=/path/to/AFLplusplus \
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_USE_UNSAFE_SUMMARIES=1 \
RZ_UNSAFE_SUMMARY_INPUT_DIR=./target/afl-release-bytes-merged \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh
```

For benchmarking this mode, aggregate the final
`[rusteze][unsafe-dflow][totals] crate=...` line for each crate across the full
build log. Looking only at the final driver crate misses most dependency-side
hook reductions.

### Running the example suite with the 3-phase analysis

To run the full examples under the same interprocedural analyze/merge/consume
pipeline, enable the wrapper flag when invoking the example runner:

```bash
RZ_INTERPROC_UNSAFE_SUMMARIES=1 \
CARGO_INCREMENTAL=0 \
python3 scripts/run_example_tests.py
```

That causes each example build to use the three-phase flow:
1. analyze-only summary build
2. offline summary merge
3. final instrumented build consuming merged summaries

Manual equivalent for a single example-style build:

```bash
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
HARNESS_TARGET_DIR=./target/example-summary \
RZ_ANALYZE_UNSAFE_SUMMARIES=1 \
RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP=1 \
cargo build

python3 ./scripts/merge_unsafe_summaries.py \
  --input-dir ./target/example-summary/rusteze-unsafe-summaries \
  --output-dir ./target/example-merged \
  --report ./target/example-merged/merge.report.txt

CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_USE_UNSAFE_SUMMARIES=1 \
RZ_UNSAFE_SUMMARY_INPUT_DIR=./target/example-merged \
cargo build
```

For normal validation of the repository, the wrapper form is the intended one:

```bash
RZ_INTERPROC_UNSAFE_SUMMARIES=1 CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py
```

## Env

Project-specific environment variables are grouped below by component/script.

### Runtime (`runtime/src/lib.rs`, `runtime/src/alias_model/*`)
- `RZ_LOG`: Runtime log level (`trace`, `info`, `warn`); default `warn`.
- `RZ_LOG_LOC`: `1/true` adds caller source location in violation output.
- `RZ_BACKTRACE_UNKNOWN_TAG`: `1/true` adds backtrace on `UNKNOWN_TAG` reports.
- `RZ_BACKTRACE`: `1/true` adds backtrace on every runtime violation.
- `RUSTEZE_FAILFAST`: non-zero panics after reporting a violation.
- `RZ_ABORT_ON_VIOLATION`: non-zero aborts process after reporting any violation.
- `RZ_ABORT_ON_DOUBLE_FREE`: non-zero aborts on `DOUBLE_FREE`.
- `RZ_STRICT_FREE_CHECK`: non-zero treats untracked `free/realloc` as violations.
- `RZ_ALLOW_UNTAGGED`: non-zero allows untagged accesses instead of hard-failing.
- `RZ_TAG0_AS_ROOT`: non-zero treats tag `0` as root in selected runtime paths.
- `RZ_LOG_ALLOC`: non-zero emits allocation event logs.
- `RZ_DUMP_ALLOC_ON_VIOLATION`: non-zero dumps allocation map on each violation (with `rz_alloc_dump` feature).
- `RZ_DUMP_ALLOC_MATCH_ADDR`: non-zero narrows alloc dump to matching addresses (with `rz_alloc_dump` feature).
- `RZ_DUMP_ALLOC_HEAP_ONLY`: non-zero limits alloc dump to heap allocations (with `rz_alloc_dump` feature).
- `RZ_ALIAS_MODEL`: alias model selector: `tb_lite` (default), `sb_lite`, `none`.
- `RZ_TB_LITE`: tree-borrows-lite on/off (`1` default, `0` disables checks inside `tb_lite` model).
- `RZ_TB_DUMP`: `1/true` adds extra TB-lite diagnostic context.
- `RZ_SB_LITE`: stacked-borrows-lite on/off (`1` default when using `sb_lite` model).
- `RZ_SB_DUMP`: `1/true` adds SB-lite stack/ancestry details in violation output.
- `RZ_STACK_REF_OOB_NOISE`: stack-ref OOB-noise suppression (`1` default, set `0` for strict reporting).
- `RZ_PROFILE_HOOKS`: `1/true` enables runtime hook profiling counters for `read`, `write`,
  `ref_create`, `raw_create`, `ptr_use`, and `record_alloc`.
- `RZ_DUMP_HOOK_PROFILE_AT_EXIT`: `1/true` dumps the aggregated runtime hook profile to stderr at
  process exit. Use this with `RZ_PROFILE_HOOKS=1` for one-shot repro/benchmark runs.

- **Global allocator wrapper (enabled by default)**: the runtime installs a
  `#[global_allocator]` wrapper around `std::alloc::System` to intercept heap
  allocations originating inside `std` / `core` / `alloc` (e.g., `Vec`, `Box`,
  `String`). This enables heap range tracking and out-of-bounds / use-after-free
  detection without instrumenting the standard library itself.

  The allocator wrapper uses thread-local reentrancy guards to avoid recursion
  and deadlocks when runtime hooks perform logging or internal allocations.

### Instrumentation pass (`instrument-mir`)
- `RZ_LOG`: Instrumentor pass log level (`trace`, `info`, `warn`); default `warn`.
- `RZ_DEBUG_MATCH`: if set, runs `debug_classify_call_effect` for that symbol and exits.
- `RZ_DEBUG_SYMBOL_LOOKUP`: non-zero enables verbose runtime-hook symbol lookup logs.
- `RZ_TRACE_PASS`: non-zero enables pass-level tracing.
- `RZ_METADATA_DATAFLOW`: enables the metadata-local dataflow optimization pass (`1` default, set
  `0` to disable). This pass only prunes redundant metadata propagation such as `TagProp`; it does
  not rewrite semantic hooks like `PtrRead` / `PtrWrite`.
- `RZ_METADATA_DATAFLOW_STATS`: non-zero prints metadata-dataflow pruning stats during
  instrumentation.
- `RZ_UNSAFE_DATAFLOW`: enables conservative unsafe-sensitive hook gating and summary computation.
- `RZ_UNSAFE_DATAFLOW_STATS`: non-zero prints unsafe-dataflow hook-pruning totals during
  instrumentation.
- `RZ_UNSAFE_DATAFLOW_SUMMARY_STATS`: non-zero prints per-function unsafe-summary statistics,
  including direct vs inherited unknown-boundary state.
- `RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP`: non-zero dumps one JSONL unsafe-summary record per analyzed
  function to `${CARGO_TARGET_DIR:-target}/rusteze-unsafe-summaries/<crate>.jsonl`.
- `RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP_PATH`: optional explicit path override for the JSONL dump file.
- `RZ_ANALYZE_UNSAFE_SUMMARIES`: analyze-only mode. The pass computes/dumps unsafe summaries and
  returns without mutating MIR or inserting runtime hooks.
- `RZ_USE_UNSAFE_SUMMARIES`: enables loading precomputed unsafe-summary JSONL files during a normal
  instrumentation build. This is intended for the second phase after running analyze-only mode and
  an offline merge step. Loaded summaries are consumed conservatively at call boundaries by the
  backward unsafe-sensitive analysis.
- `RZ_UNSAFE_SUMMARY_INPUT_DIR`: optional directory override for summary loading. Default is
  `${CARGO_TARGET_DIR:-target}/rusteze-unsafe-summaries`.
- `RZ_FILTER_STDLIB_USES`: std/core/alloc coarse-use filtering (`1` default, set `0` to disable).
- `RZ_WARN_UNKNOWN_CALLS`: unknown-call warnings (default on; set `0` to disable).
- `RZ_TRACE_UNKNOWN_CALLS`: extra unknown-call trace diagnostics.
- `RZ_HEAP_ALLOCS_FROM_MIR`: `1/true` forces MIR-level heap alloc/free hooks.
- `RZ_USE_STORAGE_DEAD`: `1/true` emits stack `live=false` on `StorageDead` (default off).
- `RZ_INSTRUMENTED_CRATES`: comma-separated dependency allowlist treated as instrumented.
- `RZ_INSTRUMENT_ALL_DEPS`: if non-zero/true, treat all *non-std-like*
  (non `std` / `core` / `alloc`) dependency crates as instrumented.
  Standard library crates are never treated as instrumented callees; instead,
  their effects are modeled via wrapper classification and allocator-boundary
  interception.
- `RZ_PRINT_CRATES`: non-zero/true prints crate graph + instrumented classification.
- `RZ_TRACE_CLASSIFY`: non-zero enables call-effect classifier tracing.
- `RZ_TRACE_CLASSIFY_FILTER`: substring filter for classify traces.
- `RZ_TRACE_CLASSIFY_LIMIT`: max traced classify events (default `50`).
- `RZ_TRACE_CLASSIFY_UNKNOWN_LIMIT`: max traced unknown-classify events (default `20`).

### `cargo-instrument-mir` wrapper (`instrument-mir/src/bin/cargo-instrument-mir.rs`)
- `CARGO`: cargo executable path (`cargo` by default).
- `RZ_DEBUG_DRIVER`: non-zero prints selected `instrument-mir` driver path.
- `RUSTFLAGS`: wrapper appends `--mir-out` / `--runtime-path` forwarding flags here.

### AFL build/fuzz scripts (`scripts/afl_*.sh`)
- `TARGET`: harness target (`bytes`, `smallvec`, `serde_json`/`serde`, `toml`, `base64`, `uuid`, `itoa`, `quick_xml`/`quick-xml`, `simd_json`/`simd-json`, `simd_json_borrowed`/`simd-json-borrowed`, `simd_json_tape`/`simd-json-tape`, `zip`, `rkyv`, `hyper`).
- `PROFILE`: cargo profile (`debug` or `release`; default depends on script, usually `release`).
- `HARNESS_TARGET_DIR`: target directory for AFL harness builds.
- `RUNTIME_FEATURES`: extra features passed when building `runtime` in `afl_build.sh`.
- `AFL_PATH`: AFL++ checkout path (used to discover `afl-fuzz` / `afl-compiler-rt.o`).
- `AFL_COMPILER_RT`: explicit path to `afl-compiler-rt.o`.
- `AFL_FUZZ`: explicit `afl-fuzz` binary path.
- `AFL_MAP_SIZE` / `AFL_MAPSIZE`: AFL map size override (Darwin default in script is `131072` when unset).
- `TIMEOUT_MS`: fuzz-case timeout passed as `afl-fuzz -t`.
- `OUT_DIR`: output dir override for `afl_repro.sh` (default `fuzz/out/<TARGET>`).
- `RZ_VERIFY_HOOKS`: post-build hook-symbol verification in `afl_build.sh` (default `1`).
- `RZ_VERIFY_HOOKS_STRICT`: strict hook verification mode in `afl_build.sh` (default `0`).
- `RZ_ALIAS_MODEL`: alias model used by fuzz/repro scripts (default `tb_lite`).
- `RZ_SB_LITE`: SB-lite runtime toggle passed by fuzz/repro scripts (default `1`).
- `RZ_INSTRUMENT_ALL_DEPS`: forced to `1` by AFL scripts.
- `RZ_INSTRUMENTED_CRATES`: optional allowlist used only when
  `RZ_INSTRUMENT_ALL_DEPS=0`.
- `RUSTFLAGS`: extended by `afl_build.sh` for sancov + `afl-compiler-rt.o` link.
- `RUSTC_TMPDIR` / `TMPDIR`: rustc temporary directory for stable same-fs behavior.
- `CARGO_TARGET_DIR`: set by scripts to isolate AFL build artifacts.
- `CARGO_INCREMENTAL`: set to `0` in `afl_build.sh`.
- `AFL_SKIP_CPUFREQ`: set to `1` by `afl_fuzz.sh`.
- `AFL_NO_AFFINITY`: set to `1` by `afl_fuzz.sh`.
- `ASAN_OPTIONS` / `ASAN_SYMBOLIZER_PATH`: explicitly unset in `afl_fuzz.sh`.
- `TRACE`: `afl_build.sh` shell tracing when `TRACE=1`.
- `scripts/merge_unsafe_summaries.py`: offline fixed-point merge for unsafe-summary JSONL dumps.
  It supports per-file mode (`--input` / `--output`) and whole-build cross-crate mode
  (`--input-dir` / `--output-dir`), plus `--report` for a human-readable propagation diff. The
  merged output is the supported input for interprocedural hook gating.
- `scripts/afl_build_interproc.sh`: native three-phase build wrapper:
  analyze-only summary pass, offline merge, then normal instrumented build consuming merged
  summaries.
- `RZ_INTERPROC_ANALYZE_TARGET_DIR`: optional analyze-pass target dir override used by
  `scripts/afl_build_interproc.sh`.
- `RZ_INTERPROC_UNSAFE_SUMMARIES`: if set to `1`, `scripts/afl_build.sh` automatically delegates
  to the three-phase interprocedural flow in `scripts/afl_build_interproc.sh`.

Recommended interprocedural native build:

```bash
AFL_PATH=/path/to/AFLplusplus \
CARGO_INCREMENTAL=0 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_INTERPROC_UNSAFE_SUMMARIES=1 \
TARGET=bytes PROFILE=release \
./scripts/afl_build.sh
```

For benchmarking, prefer aggregating `[rusteze][unsafe-dflow][totals] crate=...` across the full
build log rather than looking only at the final driver crate. Dependency crates often account for
most of the hook reduction.

### Docker AFL wrapper (`scripts/docker_afl.sh`)
- `IMAGE`: docker image tag (default `rusteze-afl`).
- `SHM_SIZE`: container shared-memory size (default `1g`).
- `USE_RUST_CACHE`: mount persistent rustup/cargo volumes (`1` default).
- `FUZZ`: if `1/true`, enables relaxed seccomp + `SYS_PTRACE`.
- `DOCKER_CPUS`: optional container CPU limit.
- `DOCKER_MEMORY`: optional container memory limit.
- `TARGET`, `PROFILE`, `AFL_COMPILER_RT`, `RUNTIME_FEATURES`, `HARNESS_TARGET_DIR`, `CARGO_TARGET_DIR`, `RUSTFLAGS`, `AFL_FUZZ`, `TIMEOUT_MS`, `OUT_DIR`: forwarded into container when set.
- All host `RZ_*`, `RUSTEZE_*`, `TRACE`, `RUST_BACKTRACE`: forwarded into container when set.

### Harness / utility scripts
- `scripts/run_harness.sh`: `CARGO`, `PROFILE` (`FAST|DEBUG`), `BUILD_PROFILE` (`debug|release`), `FLAKY_EXAMPLES`, `FLAKY_RUNS`, `REPORT_DIR`, `MEDIUM_COMMANDS_FILE`, `STOP_ON_VIOLATION`, `REINSTRUMENT`, `RZ_LOG`, `RZ_INSTRUMENT_ALL_DEPS`, `CARGO_INCREMENTAL`.
- `scripts/run_with_trace.sh`: `EXAMPLE` (required), `PROFILE`, `OUT`, `CARGO_INCREMENTAL`, `RZ_INSTRUMENT_ALL_DEPS`.
- `scripts/afl_setup.sh`: optional repo pin vars `REF_BYTES`, `REF_SMALLVEC`, `REF_SERDE`, `REF_SERDE_JSON`, `REF_TOML`, `REF_UUID`, `REF_QUICK_XML`, `REF_BASE64`, `REF_ITOA`, `REF_SIMD_JSON`, `REF_ZIP`, `REF_RKYV`, `REF_HYPER`; also reads `AFL_PATH`, `AFL_FUZZ`, `AFL_COMPILER_RT`.
- `afl_harness/src/bin/afl_smallvec_driver.rs`: `AFL_MAX_STEPS`, `AFL_MAX_LEN`.
- `instrument-mir/src/util.rs`: `HOME` is used for `~` path expansion.


## Standard Library Handling

The standard library (`std`, `core`, `alloc`) is **not instrumented directly**.
Instead, Rusteze relies on two complementary mechanisms:

1. **Wrapper classification at the MIR level** for common pointer-producing and
   memory-effect functions (e.g., `Vec::as_mut_ptr`, `ptr::add`, `ptr::copy`,
   `write_bytes`). These are modeled explicitly as pointer-derivation, read, or
   write effects.

2. **Allocator-boundary interception** via a global allocator wrapper, which
   records heap allocation and deallocation events even when they originate
   inside stdlib code.

This design avoids the need for `-Z build-std`, keeps the toolchain simple, and
ensures that pointer provenance and allocation metadata remain consistent at the
stdlib boundary.

Bugs *inside* the standard library are not instrumented instruction-by-
instruction, but misuses of stdlib APIs (e.g., out-of-bounds pointer arithmetic,
use-after-free via raw pointers) are detected at the application boundary.

### Call-argument tag buffering

To prevent unbounded memory growth when pointer-argument tags are pushed but never taken (for example, calls into uninstrumented external crates), the runtime employs a bounded ring buffer. Each pushed entry is stored in a fixed-size circular buffer, while a small hashmap maps `(callee_id, arg_index, addr)` to the buffer slot for efficient O(1) lookup on tag retrieval. When the ring buffer wraps around, overwritten entries are evicted from the hashmap, ensuring memory usage remains bounded. This design achieves O(1) push and take operations in the common case and remains robust under fuzzing and partial instrumentation.

### Runtime hot-path cache model

`__rz_ptr_read` / `__rz_ptr_write` use a tag-origin cache to avoid repeated range scans:

- At tag creation (`__record_ref_creation` / `__record_raw_ptr_creation`), the runtime snapshots:
  - `origin_base`
  - `origin_end`
  - `alloc_epoch` (existing field)
- On each access, fast path does:
  - bounds check against cached tag origin range
  - exact-base allocation lookup (`allocs.get(origin_base)`) for live/epoch checks
- Slow path is used only when cached origin is missing/invalid:
  - range lookup to find containing allocation
  - cache refresh on successful resolution

This keeps correctness fallbacks while shifting common accesses away from per-access range scans.

### Example

```bash
RZ_INSTRUMENT_ALL_DEPS=1 RZ_PRINT_CRATES=1 make instrument EXAMPLE=tag0_return_raw_ptr_from_arg
```

## Stack allocation tracking

The instrumentor records stack allocation lifetime events only for "interesting"
locals to reduce noise and overhead. A local is considered interesting when its
address is taken (e.g., via `&T` / `&raw`), or when optimized MIR introduces
common pointer-related temporaries/casts around it.

To avoid false positives from optimized MIR, we **do not honor `StorageDead`** for
address-taken locals. Instead, we emit a `live=false` event at function return
for tracked locals. This means we reliably catch use-after-return and stack
address reuse through raw pointers, but may miss some intra-function
use-after-scope cases.

At runtime, stack UAFs through **references** (`&T` / `&mut T`) are suppressed to
avoid spurious violations, while **raw-pointer** UAFs remain reported.

## Wide / Fat pointers (slices, `str`, `dyn Trait`)

Rust has *wide* pointers (a.k.a. fat pointers) that carry metadata in addition
to the data address: `&[T]` / `*const [T]` (length), `&str` / `*const str`
(length), and `&dyn Trait` / `*const dyn Trait` (vtable).

Rusteze tracks these by treating wide pointers as tag-carrying like any other
pointer, but extracting a **thin data pointer** whenever we need a concrete
address for tagging / range lookup:

- In MIR instrumentation, when a hook needs `addr = expose_provenance(ptr)`:
  - thin pointers: use `PointerExposeProvenance` directly
  - wide pointers: first cast `ptr` to `*const ()` / `*mut ()` via `PtrToPtr`
    (dropping metadata), then `PointerExposeProvenance` on that thin data
    pointer
- This keeps the runtime metadata keyed by the *data address* even when the
  source pointer is wide, so later derived thin pointers (e.g. `slice.as_ptr()`)
  keep the expected tag/epoch lineage.

Current limitations:
- We use metadata length for bounds checks and compute offsets for deref
  projections (field/index/subslice), but offsets can still be missed when
  pointer arithmetic happens in separate temporaries or through nested-deref
  patterns not reflected in a single MIR place.
- `dyn Trait` pointee sizes remain unknown; we still track the data address and
  allocation epoch, but typically use `size=0` for access checks.

Examples:
- `examples/wide_ptr_slice_uaf_read`, `examples/wide_ptr_slice_uaf_write`
- `examples/wide_ptr_dyn_trait_uaf_read`
- `examples/wide_ptr_raw_slice_cast_ok`, `examples/wide_ptr_raw_str_cast_ok`, `examples/wide_ptr_dyn_trait_cast_ok`
- Slice-length OOB panic demos (Rust bounds check triggers before runtime hook):
  - `examples/wide_ptr_slice_len_oob_read_not_detected`
  - `examples/wide_ptr_slice_len_oob_write_not_detected`

## Debugging recipes

```bash
# Show caller locations for violations
RZ_LOG=trace RZ_LOG_LOC=1 ./target/release/hello

# Include a backtrace on UNKNOWN_TAG violations
RZ_LOG=trace RZ_LOG_LOC=1 RZ_BACKTRACE_UNKNOWN_TAG=1 ./target/release/hello

# Include backtraces on any violation
RZ_LOG=trace RZ_LOG_LOC=1 RZ_BACKTRACE=1 ./target/release/hello

# Dump SB-lite stacks on SB violations
RZ_LOG=trace RZ_LOG_LOC=1 RZ_SB_DUMP=1 ./target/release/hello

# See instrumented vs dep crates during compilation
RZ_PRINT_CRATES=1 cargo instrument-mir --runtime-path=target/debug -p hello --bin hello

```

## Benchmarking / Overhead

### End-to-end wall-clock overhead (baseline vs rusteze vs ASan, optional Miri)

Use `scripts/bench_overhead.py` to build and repeatedly run binaries, producing
`reports/overhead/<timestamp>/summary.tsv` and `summary.md`.

Key knobs:
- `--suite`: `examples` | `medium` | `fuzz`
- `--targets`: optional list of `pkg` or `pkg::bin`
- `--profile`: `release` recommended for steady-state
- `--input-file`: optional file path passed as `argv[1]` to the benchmark binary
- `--include-asan`: adds an ASan build+run column (requires `cargo +nightly`)
- `--include-miri`: adds a Miri timing column (very slow; not comparable to native runtime)
- `--runs` / `--warmup`: sampling controls

Notes:
- For `afl_harness` bins (`afl_harness::afl_*_driver`), required feature flags are
  resolved automatically from Cargo metadata.
- Driver targets that read input files should be benchmarked with `--input-file`
  (for example, corpus `seed0` files under `fuzz/corpus/<target>/`).

```bash
# Examples suite (release)
python3 scripts/bench_overhead.py --suite examples --profile release --include-asan

# Medium crates (bytes + smallvec drivers)
python3 scripts/bench_overhead.py --suite medium --profile release --include-asan

# Single target
python3 scripts/bench_overhead.py --suite medium --profile release --include-asan --targets medium_bytes_driver

# Optional: include Miri timings (not comparable to native runtime)
python3 scripts/bench_overhead.py --suite medium --profile release --include-asan --include-miri --miri-runs 1

# Single AFL harness target with corpus input (4-way comparison)
python3 scripts/bench_overhead.py \
  --profile release \
  --targets afl_harness::afl_toml_driver \
  --input-file fuzz/corpus/toml/seed0 \
  --include-asan \
  --include-miri --miri-runs 1

# Multiple maintained crates (repeatable, release profile)
python3 scripts/bench_overhead.py --profile release --targets afl_harness::afl_base64_driver --input-file fuzz/corpus/base64/seed0 --include-asan --include-miri --miri-runs 1
python3 scripts/bench_overhead.py --profile release --targets afl_harness::afl_uuid_driver --input-file fuzz/corpus/uuid/seed0 --include-asan --include-miri --miri-runs 1
python3 scripts/bench_overhead.py --profile release --targets afl_harness::afl_itoa_driver --input-file fuzz/corpus/itoa/seed0 --include-asan --include-miri --miri-runs 1
python3 scripts/bench_overhead.py --profile release --targets afl_harness::afl_quick_xml_driver --input-file fuzz/corpus/quick_xml/seed0 --include-asan --include-miri --miri-runs 1
```

### Criterion microbenches (bytes/smallvec workloads)

The workspace includes a dedicated Criterion bench crate: `benchmarks/rz_bench`.

```bash
cargo bench -p rz_bench --bench bytes
cargo bench -p rz_bench --bench smallvec
```

Criterion output already includes statistical summaries. Use this for micro-level
API comparisons, and use `scripts/bench_overhead.py` for end-to-end overhead.

This crate also provides a normal binary (`rz_bench`) that can be instrumented and
timed end-to-end:

```bash
cargo run -p rz_bench --release -- --iters 100000 --which bytes
```

### Compare `rz_bench` across tools (baseline vs rusteze vs ASan, optional Miri)

Use `scripts/bench_compare_rz_bench.py` to run a fixed workload (`--iters`, `--which`)
under:
- baseline native build
- rusteze-instrumented build
- ASan build (optional)
- Miri (optional; not comparable to native runtime)

```bash
python3 scripts/bench_compare_rz_bench.py --profile release --which bytes --iters 200000 --include-asan

# Optional Miri column (very slow)
python3 scripts/bench_compare_rz_bench.py --profile release --which bytes --iters 200000 --include-asan --include-miri --miri-runs 1
```

This writes:
- `reports/bench_compare/<timestamp>/summary.tsv`
- `reports/bench_compare/<timestamp>/result.json` (full samples and build commands)

## AFL++ (Native vs Docker)

The repo includes an AFL++ harness crate with drivers:
`afl_bytes_driver`, `afl_smallvec_driver`, `afl_serde_json_driver`,
`afl_toml_driver`, `afl_base64_driver`, `afl_uuid_driver`,
`afl_itoa_driver`, `afl_quick_xml_driver`, `afl_simd_json_driver`,
`afl_zip_driver`, `afl_rkyv_driver`, and `afl_hyper_driver`.

Bootstrap third-party checkouts and env hints first:

```bash
./scripts/afl_setup.sh
```

On macOS, AFL++ shared-memory can be unreliable; the recommended workflow is to
use the Linux container described in `docker/afl/README.md`.

**Native (Linux)**

```bash
# Build the harness (requires AFL++ runtime object `afl-compiler-rt.o`)
export AFL_PATH=/path/to/AFLplusplus   # or set AFL_COMPILER_RT=/path/to/afl-compiler-rt.o
TARGET=bytes PROFILE=release ./scripts/afl_build.sh
TARGET=serde_json PROFILE=release ./scripts/afl_build.sh
TARGET=simd_json PROFILE=release ./scripts/afl_build.sh
TARGET=zip PROFILE=release ./scripts/afl_build.sh
TARGET=rkyv PROFILE=release ./scripts/afl_build.sh
TARGET=hyper PROFILE=release ./scripts/afl_build.sh

# Run AFL++
TARGET=bytes PROFILE=release ./scripts/afl_fuzz.sh
TARGET=serde_json PROFILE=release ./scripts/afl_fuzz.sh
TARGET=simd_json PROFILE=release ./scripts/afl_fuzz.sh
TARGET=hyper PROFILE=release ./scripts/afl_fuzz.sh

# Reproduce a single crash
TARGET=bytes PROFILE=release ./scripts/afl_repro.sh --input fuzz/out/bytes/default/crashes/id:...
TARGET=serde_json PROFILE=release ./scripts/afl_repro.sh --input fuzz/out/serde_json/default/crashes/id:...
TARGET=simd_json PROFILE=release ./scripts/afl_repro.sh --input fuzz/out/simd_json/default/crashes/id:...
TARGET=hyper PROFILE=release ./scripts/afl_repro.sh --input fuzz/out/hyper/default/crashes/id:...
```

`scripts/afl_build.sh` includes a post-build hook check and prints:
`[rusteze] hook check: found rusteze hooks (__rz_*) ...` when instrumentation hooks are present.

For strict CI-style verification:

```bash
RZ_VERIFY_HOOKS=1 RZ_VERIFY_HOOKS_STRICT=1 TARGET=bytes PROFILE=debug ./scripts/afl_build.sh
```

**Docker (Linux container)**

See `docker/afl/README.md` for the container workflow. The scripts above work inside
the container as well (with AFL++ preinstalled in the image).

After updating `docker/afl/Dockerfile`, rebuild the image:

```bash
docker build -t rusteze-afl -f docker/afl/Dockerfile .
```

You can also pass env vars directly through the helper:

```bash
./scripts/docker_afl.sh TARGET=serde_json PROFILE=release ./scripts/afl_build.sh
```

`scripts/docker_afl.sh` now forwards host `RZ_*` / `RUSTEZE_*` / `TRACE` vars, so
instrumentation diagnostics are visible inside the container. Example:

```bash
RZ_PRINT_CRATES=1 RZ_TRACE_PASS=1 TRACE=1 ./scripts/docker_afl.sh TARGET=itoa PROFILE=debug ./scripts/afl_build.sh
```


## Notes
We can force loading extern crate with

```
--extern=force:runtime={runtime_path}/libruntime.rlib
```

- Performance: For best performance, disable tracing (`RZ_LOG=warn` or unset). Future
  work includes further cache tuning, reducing hot-path hook density in MIR, and
  optional native runtime backends for hot paths.
