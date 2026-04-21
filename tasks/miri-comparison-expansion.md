# Fair Miri Comparison Expansion

## Goal

Build a fair, publishable Miri comparison for `rusteze`.

Fair means:
- compare against **exact Miri ports**, not "inspired by" tests
- report **percentage agreement** on those exact ports
- separate:
  - exact Miri ports
  - Miri-inspired regressions
  - broader repo example suite
- grow coverage not only for borrowing rules, but also for other Miri classes that match
  `rusteze` goals: provenance, dangling/stale pointers, allocation lifetime, unaligned access,
  uninit, and call-boundary transport bugs

Do **not** mix these categories in a single headline number.

## Why

Current numbers are easy to misread:
- exact Miri ports in dedicated dirs: `52`
- Miri-inspired but not exact ports: `13`
- total Miri-related tests: `55`
- total bins in mixed micro suites are not headline metric

Current exact-port comparison:
- `52 / 52 = 100.0%` agreement
- report: `reports/miri_compare/20260421_220831/summary.tsv`

This is honest, but sample too small.

BorrowSanitizer's reported `405` is **not** "all Miri tests".
It is their filtered relevant subset from Miri's suite.
We need our own filtered exact-port number, much larger than `15`.

Current disagreement set:
- none in the current exact-port set

Next disagreement work should target these new TB-specific exact ports first.

## Required Output

Produce one headline metric in repo:

`rusteze agrees with Miri on X / Y exact Miri ports = Z%`

Where:
- `Y` counts only exact ports from `miri/tests/fail/...`
- `X` counts exact behavioral agreement under current comparison script
- disagreements are listed explicitly by test name

## Current Infrastructure

Already in repo:
- exact-port marker:
  - source comment form:
    - `// Ported from miri/tests/fail/...`
- comparison script:
  - `scripts/run_miri_comparison.py`
- dedicated exact-port suites:
  - `examples/miri_tests/sb_exact`
  - `examples/miri_tests/tb_exact`
- inspired/regression suites:
  - `examples/sb_miri_micro`
  - `examples/tb_miri_micro`

## Rules

1. New comparison inputs must be **exact ports** first.
2. Avoid adding new "inspired by" tests for comparison headline.
3. Keep exact ports small and deterministic.
4. Prefer thin-pointer / no-dependency / no-Miri-specific-feature tests first.
5. Record current `rusteze` behavior honestly in expected files if gap still exists.
6. Do not silently reclassify gaps as unsupported unless there is a concrete reason.

## Expansion Plan

### Phase 1: Grow Exact Port Set Fast

Target families first:
- `miri/tests/fail/both_borrows/*`
- `miri/tests/fail/tree_borrows/*`
- `miri/tests/fail/stacked_borrows/*`

Also target exact ports from other Miri classes that are directly relevant to `rusteze`:
- `miri/tests/fail/provenance/*`
- `miri/tests/fail/dangling_pointers/*`
- `miri/tests/fail/alloc/*`
- `miri/tests/fail/unaligned_pointers/*`
- selected `miri/tests/fail/uninit/*`
- selected `miri/tests/fail/function_calls/*`

Prioritize tests that are:
- single-file
- no external deps
- no Miri-only APIs
- no panic-as-oracle
- easy to port into `miri_sb_exact` / `miri_tb_exact`

Best candidates:
- more `pass_invalid_*`
- more `return_invalid_*`
- `load_invalid_*`
- `aliasing_mut*`
- more simple protector / fn-entry / read-vs-write cases
- provenance / dangling / alloc / unaligned cases with direct memory-safety signal

Priority order for this repo:
1. `tree_borrows`
2. `both_borrows`
3. memory-safety/provenance classes:
   - `provenance`
   - `dangling_pointers`
   - `alloc`
   - `unaligned_pointers`
   - selected `uninit`
   - selected `function_calls`
4. only selected `stacked_borrows` cases that are still meaningful for Tree Borrows or
   general memory safety

### Phase 2: Classify Ports

For each exact port:
- `ported`
- `rusteze` expected behavior recorded
- included in `scripts/run_miri_comparison.py`

For each disagreement:
- classify reason:
  - missing instrumentation
  - missing runtime metadata
  - alias-model mismatch
  - unsupported wildcard / exposed / wide-pointer semantics
  - expectation bug

### Phase 3: Raise Agreement

Attack highest-yield disagreement clusters first:
- tree-borrows interior-mutability / lazy-conflict gaps:
  `cell_inside_struct`, `repeated_foreign_read_lazy_conflicted`,
  `reservedim_spurious_write`, `spurious_read`
- import more exact ports beyond the current `42`
- keep exact-port agreement high while growing beyond `50+`
- separate exact-port growth from older inspired/regression-suite cleanup
- once current TB disagreement set shrinks, expand into provenance / dangling / alloc /
  unaligned classes instead of padding with low-value SB-specific tests

## Milestones

### Milestone A

Reach:
- `>= 25` exact Miri ports

Deliver:
- updated `reports/miri_compare/.../summary.tsv`
- updated top-line percentage

### Milestone B

Reach:
- `>= 50` exact Miri ports

Deliver:
- disagreement buckets
- per-family pass rates

### Milestone C

Reach:
- stable publishable metric on exact ports

Preferred:
- `>= 70%` on `>= 50` exact ports

Stronger:
- `>= 80%` on `>= 50` exact ports

## Reporting Format

Always report 3 numbers separately:

1. exact Miri ports:
   - `X / Y = Z%`
2. Miri-inspired regressions:
   - count only, no head-to-head claim
3. full repo example suite:
   - pass rate only

Example:
- exact Miri ports: `31 / 52 = 59.6%`
- Miri-inspired regressions: `13`
- full example suite: `109 / 109`

## Stop Conditions

Do **not** stop after adding tests only.
Stop only when all are true:
- exact-port set meaningfully larger than `21`
- percentage agreement recomputed
- disagreement list shortened or at least categorized
- metric ready for paper/preprint text

## Commands

Run exact-port comparison:

```bash
python3 scripts/run_miri_comparison.py
```

Run exact-port additions incrementally:

```bash
EXAMPLE_FILTER='^miri_sb_exact::NEW_TEST$' RZ_ALIAS_MODEL=sb_lite RECORD_EXPECT=1 python3 scripts/run_example_tests.py
EXAMPLE_FILTER='^miri_tb_exact::NEW_TEST$' RZ_ALIAS_MODEL=tb_lite RECORD_EXPECT=1 python3 scripts/run_example_tests.py
```

## Non-Goals

Not goal here:
- fuzzing
- ASan comparison
- performance benchmarking
- new inspired-by tests for paper headline

Also not goal:
- bulk-importing all `stacked_borrows/*` tests just to inflate port count
- broad Miri feature coverage unrelated to aliasing, provenance, or memory safety

This task only exists to make Miri comparison fair and publishable.
