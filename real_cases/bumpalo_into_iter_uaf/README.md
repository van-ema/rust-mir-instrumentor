# `bumpalo` historical iterator use-after-free case

This case reproduces the historical `bumpalo` bug in vulnerable version
`3.11.0`, where iterating a `bumpalo::collections::Vec` after dropping the
underlying arena yields a dangling iterator.

## Crate

- crate: `bumpalo`
- pinned version: `3.11.0`

## Repro

The reproducer follows the published pattern:

1. allocate a `Vec<u8>` inside a `Bump`
2. call `into_iter()`
3. drop the `Bump`
4. iterate the returned `IntoIter`

The iterator still points into arena memory that has already been destroyed.

## Expected baseline behavior

Captured artifact:

- `real_cases/bumpalo_into_iter_uaf/artifacts/baseline.txt`

Run:

```bash
cargo run --manifest-path real_cases/bumpalo_into_iter_uaf/Cargo.toml -q
```

Observed on the current macOS setup:

- the process does not crash
- it prints stale zero bytes from freed memory
- exit code is `0`

Example:

```text
0x00 0x00 0x00 0x00 ...
EXIT:0
```

## Miri

Captured artifact:

- `real_cases/bumpalo_into_iter_uaf/artifacts/miri.txt`

Run:

```bash
cargo miri run --manifest-path real_cases/bumpalo_into_iter_uaf/Cargo.toml
```

Observed:

- Miri reports Undefined Behavior
- the failing operation is iterator pointer arithmetic on freed arena memory

## Rusteze

Captured artifact:

- `real_cases/bumpalo_into_iter_uaf/artifacts/rusteze.txt`

Build:

```bash
PATH="$PWD/target/debug:$PATH" \
RZ_INSTRUMENT_ALL_DEPS=1 \
CARGO_INCREMENTAL=0 \
cargo instrument-mir \
  --runtime-path="$PWD/target/release" \
  --manifest-path real_cases/bumpalo_into_iter_uaf/Cargo.toml
```

Run:

```bash
RUSTEZE_FAILFAST=1 \
RZ_ABORT_ON_VIOLATION=1 \
real_cases/bumpalo_into_iter_uaf/target/debug/bumpalo_into_iter_uaf
```

Observed:

```text
================ RUSTEZE VIOLATION ================
USE_AFTER_DEAD
READ via tag=... addr=... size=1
alloc_base=... alloc_epoch=... kind=RawConst ...
===================================================
```

Exit code:

- `134`

## Paper angle

This case study supports the claim that:

- Rusteze detects a real historical Rust use-after-free under native execution
- Miri agrees on the underlying bug when the target is otherwise runnable
- native baseline execution can silently continue with stale data, so the bug is
  not self-reporting without dynamic checking
