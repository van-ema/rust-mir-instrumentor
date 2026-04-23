# miri_tb_pass_exact

Exact ports from `miri/tests/pass/tree_borrows/**` and
`miri/tests/pass/both_borrows/**`.

Purpose:
- false-positive coverage for valid Tree-Borrows programs
- fair head-to-head comparison with Miri pass cases
- no "inspired by" tests mixed into headline percentage

Expectation files record current rusteze behavior so the normal example suite stays usable. For
these pass tests, the authoritative false-positive signal is the Miri comparison summary: any row
with `miri_class=ok`, `rusteze_class=violation`, and `agree=no` is a rusteze false positive to
investigate.

Main comparison command:

```bash
python3 scripts/run_miri_comparison.py
```
