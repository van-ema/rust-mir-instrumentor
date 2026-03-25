# `smallvec` historical `insert_many` out-of-bounds case

This case reproduces the historical `smallvec` bug in vulnerable version
`1.6.0`, where `SmallVec::insert_many` can perform out-of-bounds operations
when used with a zero-inline-capacity vector.

## Crate

- crate: `smallvec`
- pinned version: `1.6.0`

## Repro

The reproducer follows the published pattern:

1. create `SmallVec<[u8; 0]>`
2. push one element so heap storage is allocated
3. call `insert_many(0, iter)` with a filtered iterator whose lower size hint is
   misleadingly small
4. observe invalid internal pointer movement during insertion

## Expected baseline behavior

Captured artifact:

- `real_cases/smallvec_insert_many_oob/artifacts/baseline.txt`

Run:

```bash
cargo run --manifest-path real_cases/smallvec_insert_many_oob/Cargo.toml -q
```

Observed on the current macOS setup:

- the program prints `Hello!`
- then panics in the standard library while dropping the corrupted `SmallVec`
- the process aborts with exit code `134`

This is not a clean crate-local report. The modern toolchain trips a later
standard-library UB precondition during teardown.

## Miri

Captured artifact:

- `real_cases/smallvec_insert_many_oob/artifacts/miri.txt`

Run:

```bash
cargo miri run --manifest-path real_cases/smallvec_insert_many_oob/Cargo.toml
```

Observed:

- Miri reports Undefined Behavior
- the first failing operation is an out-of-bounds `ptr::copy` inside
  `smallvec::SmallVec::insert_many`
- the report points at `smallvec-1.6.0/src/lib.rs:1048`

## Rusteze

Captured artifact:

- `real_cases/smallvec_insert_many_oob/artifacts/rusteze.txt`

Build:

```bash
PATH="$PWD/target/debug:$PATH" \
RZ_INSTRUMENT_ALL_DEPS=1 \
CARGO_INCREMENTAL=0 \
cargo instrument-mir \
  --runtime-path="$PWD/target/release" \
  --manifest-path real_cases/smallvec_insert_many_oob/Cargo.toml
```

Run:

```bash
RUSTEZE_FAILFAST=1 \
RZ_ABORT_ON_VIOLATION=1 \
RZ_LOG_LOC=1 \
RZ_BACKTRACE=1 \
real_cases/smallvec_insert_many_oob/target/debug/smallvec_insert_many_oob
```

Observed:

```text
================ RUSTEZE VIOLATION ================
OUT_OF_BOUNDS
WRITE via tag=... addr=... size=1
loc=.../smallvec-1.6.0/src/lib.rs:1054:17
===================================================
```

Exit code:

- `134`

## Paper angle

This case study supports the claim that:

- Rusteze detects a real historical out-of-bounds bug in a widely used Rust
  crate under native execution
- Miri agrees on the underlying spatial violation
- native baseline execution does not provide a clean crate-local diagnosis,
  while Rusteze reports the invalid write directly at the vulnerable site
