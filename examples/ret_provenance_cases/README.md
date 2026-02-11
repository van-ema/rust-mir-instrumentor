# ret_provenance_cases

Small inter-procedural pointer-return micro-cases.

Each `src/bin/*.rs` is a standalone scenario meant to stress call-boundary
return provenance and alias tracking.

Naming convention:
- `*_ub.rs`: scenario is expected to trigger a violation.
- `*_ok.rs`: scenario is expected to run without violations.

Expected outcomes for the test harness are in `expected.<bin>.rz`.
