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

## Env

### Runtime
- `RZ_LOG`: Log level for runtime output (`trace`, `info`, `warn`); default is `warn`.
- `RZ_LOG_LOC`: If set to `1` or `true`, include caller location information in runtime violation logs.
- `RZ_BACKTRACE_UNKNOWN_TAG`: If set to `1` or `true`, include a backtrace on `UNKNOWN_TAG` violations.
- `RUSTEZE_FAILFAST`: If non-zero, panic on violation; default is log-and-continue.
- `RZ_ABORT_ON_DOUBLE_FREE`: If non-zero, abort the process on `DOUBLE_FREE` after reporting.
- `RZ_STRICT_FREE_CHECK`: If non-zero, treat frees of unknown bases as violations (and skip system dealloc).

- **Global allocator wrapper (enabled by default)**: the runtime installs a
  `#[global_allocator]` wrapper around `std::alloc::System` to intercept heap
  allocations originating inside `std` / `core` / `alloc` (e.g., `Vec`, `Box`,
  `String`). This enables heap range tracking and out-of-bounds / use-after-free
  detection without instrumenting the standard library itself.

  The allocator wrapper uses thread-local reentrancy guards to avoid recursion
  and deadlocks when runtime hooks perform logging or internal allocations.

### Instrumentation
- `RZ_LOG`: Log level for the instrumentor pass (`trace`, `info`, `warn`); default is `warn`.
- `RZ_INSTRUMENTED_CRATES`: Comma-separated list of dependency crate names to treat as instrumented for call-boundary tag passing.
- `RZ_INSTRUMENT_ALL_DEPS`: If non-zero/true, treat all *non-std-like*
  (non `std` / `core` / `alloc`) dependency crates as instrumented.
  Standard library crates are never treated as instrumented callees; instead,
  their effects are modeled via wrapper classification and allocator-boundary
  interception.
- `RZ_PRINT_CRATES`: If non-zero/true, print the crate graph and show which crates are instrumented.
- `RZ_FILTER_STDLIB_USES`: If set to `0` or `false`, do not filter out coarse pointer-use hooks from std/core/alloc; default is enabled.
- `RZ_WARN_UNKNOWN_CALLS`: If set to `0` or `false`, suppress warnings about unknown direct calls with pointer effects; default is enabled.
- `RZ_HEAP_ALLOCS_FROM_MIR`: If set to `1` or `true`, emit MIR-level heap alloc/free hooks (overrides allocator-wrapper default).
- `RZ_DEBUG_SYMBOL_LOOKUP`: If set to `1` or `true`, print verbose symbol lookup logs when resolving runtime hooks.


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

## Debugging recipes

```bash
# Show caller locations for violations
RZ_LOG=trace RZ_LOG_LOC=1 ./target/release/hello

# Include a backtrace on UNKNOWN_TAG violations
RZ_LOG=trace RZ_LOG_LOC=1 RZ_BACKTRACE_UNKNOWN_TAG=1 ./target/release/hello

# See instrumented vs dep crates during compilation
RZ_PRINT_CRATES=1 cargo instrument-mir --runtime-path=target/debug -p hello --bin hello

```

## AFL++ (Docker)

If you want AFL++ tooling on Linux (useful for crash reproduction and minimization),
see `docker/afl/README.md`. This is especially handy on macOS hosts where AFL++
shared-memory can be unreliable.



## Notes
We can force loading extern crate with

```
--extern=force:runtime={runtime_path}/libruntime.rlib
```

- Performance:For best performance, disable tracing (`RZ_LOG=warn` or unset). Future
  work includes lock sharding, TLS fast paths,
  and optional native runtime backends for hot paths.
