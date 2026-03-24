# Real Cases

This directory contains standalone historical bug repros used as case studies
for the Rusteze paper.

Each case is intentionally kept out of the main workspace and pins a vulnerable
crate version so the behavior remains reproducible.

Current cases:

- `slice_ring_buffer_double_free`
- `bumpalo_into_iter_uaf`
