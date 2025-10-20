# rust-mir-instrumentor

## Install
```
cargo build --release
cargo install --path . --bins
```

## Use
```
RUSTC_WRAPPER=instrument-mir cargo build
```