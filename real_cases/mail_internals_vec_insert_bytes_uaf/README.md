# mail_internals_vec_insert_bytes_uaf

Historical `mail-internals` `0.2.0` bug corresponding to `RUSTSEC-2023-0054`.

The vulnerable helper is:

- `mail_internals::utils::vec_insert_bytes`

It saves raw pointers to the source slice and destination insertion point before
calling `Vec::reserve`. When the source slice aliases the destination vector,
`reserve` may reallocate and free the old buffer while those pointers remain in
use. The subsequent writes operate on stale provenance.

This in-tree case vendors `mail-internals` `0.2.0` and applies only a minimal
compatibility patch for current nightly Rust:

- disambiguate `type_id` in `src/encoder/encodable.rs`

That patch does not change the vulnerable `vec_insert_bytes` logic.

## Expected behavior

- Baseline native execution: silent, prints `ABABCDCD`
- Miri: reports UB during `target.reserve(insertion_len)`
- Rusteze: reports `TREE_BORROWS_VIOLATION` with `TB_LITE_FROZEN_WRITE`

## Reproduce

Baseline:

```bash
cargo run --manifest-path real_cases/mail_internals_vec_insert_bytes_uaf/Cargo.toml -q
```

Miri:

```bash
cargo miri run --manifest-path real_cases/mail_internals_vec_insert_bytes_uaf/Cargo.toml
```

Rusteze:

```bash
PATH="$PWD/target/debug:$PATH" \
RZ_INSTRUMENT_ALL_DEPS=1 \
CARGO_INCREMENTAL=0 \
CARGO_TARGET_DIR=target/rusteze \
cargo instrument-mir \
  --runtime-path="$PWD/target/release" \
  --manifest-path real_cases/mail_internals_vec_insert_bytes_uaf/Cargo.toml

RUSTEZE_FAILFAST=1 \
RZ_ABORT_ON_VIOLATION=1 \
target/rusteze/debug/mail_internals_vec_insert_bytes_uaf
```

## Artifacts

- `artifacts/baseline.txt`
- `artifacts/miri.txt`
- `artifacts/rusteze.txt`
