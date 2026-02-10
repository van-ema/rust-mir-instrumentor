# ret_provenance_cases

Small inter-procedural pointer-return micro-cases.

Each `src/bin/*.rs` is a standalone scenario meant to stress call-boundary
return provenance and alias tracking.

Expected outcomes for the test harness are in `expected.<bin>.rz`.

To compare behavior with/without return-tag plumbing:

- default (current stable baseline):
  - `RZ_ENABLE_RETTAKE=0`
  - `RZ_ENABLE_RETPUSH=0`
- stronger inter-procedural path:
  - `RZ_ENABLE_RETTAKE=1`
  - `RZ_ENABLE_RETPUSH=1`
