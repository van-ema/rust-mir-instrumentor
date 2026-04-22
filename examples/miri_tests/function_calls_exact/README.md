# miri_function_calls_exact

Exact ports from `miri/tests/fail/function_calls/**` that are relevant to
`rusteze`'s call-boundary aliasing and pointer-transport checks.

Purpose:
- fair head-to-head comparison with Miri
- no "inspired by" tests mixed into headline percentage

Main comparison command:

```bash
python3 scripts/run_miri_comparison.py
```
