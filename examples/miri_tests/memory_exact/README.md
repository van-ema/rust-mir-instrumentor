# miri_mem_exact

Exact ports from `miri/tests/fail/{dangling_pointers,alloc,provenance,unaligned_pointers,...}`.

Purpose:
- fair head-to-head comparison with Miri for memory-safety failures outside borrow-only rules
- keep these ports separate from `sb_exact` and `tb_exact`

Main comparison command:

```bash
python3 scripts/run_miri_comparison.py
```
