# slab_get2_unchecked_mut_alias

Historical `slab` aliasing bug based on the upstream `0.4.5 -> 0.4.6` fix
`Fix stacked borrows violation in get2_unchecked_mut (#115)`.

This case vendors `slab` `v0.4.5` and calls the unsafe API:

- `Slab::get2_unchecked_mut`

The vulnerable implementation obtains two mutable references by calling
`self.entries.get_unchecked_mut(...)` twice on the same backing vector,
which Miri flags as a Stacked Borrows violation. The upstream fix rewrites
that code to derive both pointers from a single raw base pointer instead.

## Expected behavior

- Baseline native execution: silent, prints `2 1`
- Miri: reports a Stacked Borrows retag violation in `slab::get2_unchecked_mut`

## Reproduce

Baseline:

```bash
cargo run --manifest-path real_cases/slab_get2_unchecked_mut_alias/Cargo.toml -q
```

Miri:

```bash
cargo miri run --manifest-path real_cases/slab_get2_unchecked_mut_alias/Cargo.toml
```

## Artifacts

- `artifacts/baseline.txt`
- `artifacts/miri.txt`
