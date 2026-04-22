# miri_provenance_exact

Exact ports from `miri/tests/fail/provenance/**` that exercise concrete
pointer-forgery / no-provenance dereference behavior relevant to `rusteze`.

Purpose:
- fair head-to-head comparison with Miri
- no "inspired by" tests mixed into headline percentage

Main comparison command:

```bash
python3 scripts/run_miri_comparison.py
```
