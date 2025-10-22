# rust-mir-instrumentor

## Build
```
cargo build --release -p runtime
cargo install --path instrument-mir --bin instrument-mir
cargo install --path instrument-mir --bin cargo-instrument-mir
```

## Use

```
cargo instrument-mir --bin hello
```

Linux:
```
DYLD_FALLBACK_LIBRARY_PATH="$(rustc --print sysroot)/lib"
LD_LIBRARY_PATH="$(rustc --print sysroot)/lib" \
target/release/instrument-mir \
    --crate-name hello \
    examples/hello/src/main.rs \
    --crate-type=bin \
    --extern runtime=./target/release/libruntime.rlib \
    -L target/release \
    -o hello_instrumented
```

on MacOs
```
DYLD_FALLBACK_LIBRARY_PATH="$(rustc --print sysroot)/lib"
target/release/instrument-mir \
    --crate-name hello \
    examples/hello/src/main.rs \
    --crate-type=bin \
    --extern runtime=./target/release/deps/libruntime-da7beaa1ec0bf9aa.rmeta \
    -L target/release \
    -o hello_instrumented
```