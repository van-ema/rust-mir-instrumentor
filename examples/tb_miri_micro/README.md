# tb_miri_micro

Small, deterministic micro-tests inspired by `miri/tests/fail/tree_borrows/**`.

These tests are intended to be run in both alias models:
- `RZ_ALIAS_MODEL=sb_lite`
- `RZ_ALIAS_MODEL=tb_lite`

Per-model expectations are stored as `expected.<bin>.<model>.rz`.

Protector-focused bins:
- `protected_raw_write`: raw write while argument protector is active.
- `protected_dealloc`: deallocation while argument protector is active.
- `fnentry_invalidation`: write through stale raw after fn-entry retag.
- `protector_write_lazy`: lazy pointer pattern with protector-end sensitivity.
- `reservedim_spurious_write`: simplified interior-mutability/Reserved interaction.

Exact Miri ports now live in:
- `examples/miri_tests/tb_exact`
