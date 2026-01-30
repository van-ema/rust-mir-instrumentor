# sb_miri_micro

Small, deterministic micro-tests inspired by `miri/tests/fail/**` that exercise the
SB-lite ("Stacked Borrows lite") checking in rusteze.

## How it works

- Each file in `src/bin/*.rs` is a standalone test program.
- Expected outcomes live in `expected.<bin>.rz` and are compared by
  `scripts/run_example_tests.py`.
  - Use `ok` if no violation should be reported.
  - Otherwise use the signature format emitted by the test runner:
    `KIND|ACCESS|POINTER_KIND|SIZE`

## Adding a new test

1. Add a new binary under `examples/sb_miri_micro/src/bin/<name>.rs`.
2. Add `examples/sb_miri_micro/expected.<name>.rz`.
3. Run `python3 scripts/run_example_tests.py` from the repo root.

## Notes / current limitations

- Prefer **thin pointers / sized pointees** in these micro-tests.
  The current pointer-tagging is intentionally conservative around wide pointers
  (e.g., `&[T]`, `&str`), so tests that depend on wide-pointer lineage may need
  extra modeling work in the instrumentation pass.

