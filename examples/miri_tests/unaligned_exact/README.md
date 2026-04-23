# miri_unaligned_exact

Exact ports from `miri/tests/fail/unaligned_pointers/**` that exercise
misaligned references and raw-pointer accesses relevant to `rusteze`.

Purpose:
- fair head-to-head comparison with Miri
- no "inspired by" tests mixed into headline percentage

Main comparison command:

```bash
python3 scripts/run_miri_comparison.py
```
