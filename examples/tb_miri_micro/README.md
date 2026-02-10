# tb_miri_micro

Small, deterministic micro-tests inspired by `miri/tests/fail/tree_borrows/**`.

These tests are intended to be run in both alias models:
- `RZ_ALIAS_MODEL=sb_lite`
- `RZ_ALIAS_MODEL=tb_lite`

Per-model expectations are stored as `expected.<bin>.<model>.rz`.
