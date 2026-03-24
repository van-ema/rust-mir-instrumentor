# `slice-ring-buffer` historical double-free case

This case reproduces the historical `slice-ring-buffer` safe-API memory bug in
vulnerable version `0.3.4`.

## Crate

- crate: `slice-ring-buffer`
- pinned version: `0.3.4`

## Repro

The reproducer mirrors a published proof-of-concept pattern:

1. create a `SliceRingBuffer`
2. push one owned value
3. pop it
4. extend from another slice

The vulnerable implementation mishandles ownership, which leads to repeated
destruction of the same backing object.

## Expected baseline behavior

Run:

```bash
cargo run --manifest-path real_cases/slice_ring_buffer_double_free/Cargo.toml -q
```

Observed on macOS:

- the same heap pointer is printed multiple times by `Drop`
- process aborts with exit code `133`

Example:

```text
pushed
Dropping StructA with data at: 0x104b59c10
popped
Dropping StructA with data at: 0x104b59c10
extended: [StructA("BBBB")]
Dropping StructA with data at: 0x104b59c10
EXIT:133
```

## Miri

Run:

```bash
cargo miri run --manifest-path real_cases/slice_ring_buffer_double_free/Cargo.toml
```

Observed on macOS:

- Miri does not reach the bug
- it fails earlier because `slice-ring-buffer` uses unsupported Mach VM
  operations on this platform

So this case is useful precisely because Rusteze can run it natively while Miri
is unavailable here.

## Rusteze

Build:

```bash
PATH="$PWD/target/debug:$PATH" \
RZ_INSTRUMENT_ALL_DEPS=1 \
CARGO_INCREMENTAL=0 \
cargo instrument-mir \
  --runtime-path="$PWD/target/release" \
  --manifest-path real_cases/slice_ring_buffer_double_free/Cargo.toml
```

Run:

```bash
RUSTEZE_FAILFAST=1 \
RZ_ABORT_ON_VIOLATION=1 \
real_cases/slice_ring_buffer_double_free/target/debug/slice_ring_buffer_double_free
```

Observed:

```text
================ RUSTEZE VIOLATION ================
WILD_POINTER
WRITE via tag=163 addr=0x100e9c000 size=24
(no allocation contains this address) kind=RawMut parent=0 pointee=0x100e9c000
===================================================
```

Exit code:

- `134`

## Paper angle

This case study supports the claim that:

- Rusteze can detect a real historical Rust memory-safety bug under native
  execution
- and can do so on a target/platform where Miri is not practically usable due
  unsupported runtime operations
