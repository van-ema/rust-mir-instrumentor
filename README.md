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

## Env
### Runtime

- **`RUSTEZE_FAILFAST`**: control violation handling
  - unset / `0` → log and continue (default)
  - non-zero → panic (recommended for fuzzing)

  ```bash
  RUSTEZE_FAILFAST=1 RUST_BACKTRACE=1 ./target/debug/my_binary
  ```

### Instrumentation

- **`RZ_STACK_ALLOCS`**: stack allocation tracking scope
  - unset → only "interesting" locals (default)
  - `all`, `1`, `true` → all locals

  ```bash
  RZ_STACK_ALLOCS=all cargo instrument-mir ...
  ```

- **`RZ_INSTRUMENTED_CRATES`**: comma-separated allowlist of dependency crate names to treat as instrumented for call-boundary tag passing (in addition to the current crate).
- **`RZ_INSTRUMENT_ALL_DEPS`**: when non-zero/true, treat all non-std/non-runtime dependency crates as instrumented for call-boundary tag passing.
- **`RZ_PRINT_CRATES`**: when non-zero/true, print the crate graph once during compilation and indicate which crates are considered instrumented.

### Call-argument tag buffering

To prevent unbounded memory growth when pointer-argument tags are pushed but never taken (for example, calls into uninstrumented external crates), the runtime employs a bounded ring buffer. Each pushed entry is stored in a fixed-size circular buffer, while a small hashmap maps `(callee_id, arg_index, addr)` to the buffer slot for efficient O(1) lookup on tag retrieval. When the ring buffer wraps around, overwritten entries are evicted from the hashmap, ensuring memory usage remains bounded. This design achieves O(1) push and take operations in the common case and remains robust under fuzzing and partial instrumentation.

### Example

```bash
RZ_INSTRUMENT_ALL_DEPS=1 RZ_PRINT_CRATES=1 make instrument EXAMPLE=tag0_return_raw_ptr_from_arg
```

## Stack allocation tracking (filtered vs all)

By default, the instrumentor records stack allocation lifetime events (`StorageLive` / `StorageDead`) **only for "interesting" locals** to reduce noise and overhead. A local is considered interesting when its address is taken (e.g., via `&T` / `&raw`), or when optimized MIR introduces common pointer-related temporaries/casts around it.

If you want maximum coverage (useful when debugging or when validating against tricky MIR patterns), you can force the instrumentor to record `StorageLive` / `StorageDead` for **all locals** by setting `RZ_STACK_ALLOCS`:

```bash

# Instrument stack allocs for all locals (higher overhead, more logs)
RZ_STACK_ALLOCS=all cargo instrument-mir ...

# Equivalent values:
RZ_STACK_ALLOCS=1 cargo instrument-mir ...
RZ_STACK_ALLOCS=true cargo instrument-mir ...
```

Notes:
- The toggle affects only stack allocation lifetime tracking; pointer creation/use tracking is unchanged.
- The "all" mode can produce many more `__rz_record_alloc` events, especially in code that uses formatting/panic paths.



## Notes
We can force loading extern crate with

```
--extern=force:runtime={runtime_path}/libruntime.rlib
```
