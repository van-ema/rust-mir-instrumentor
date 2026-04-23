# Phase 1 — TB protector-conflict widening

## Scope

Triage of 16 example-suite mismatches observed under `tb_lite` default mode on branch `miri-exact-comparison-fixes`. Phase 1 target was the **Bucket A** cluster: TB violations expected by tests but not emitted by rusteze.

Starting state: `190 ok / 16 mismatch` (`reports/example_tests/20260423_102829/summary.tsv`).
Final state: `191 ok / 15 mismatch`.

## Code changes

### `runtime/src/alias_model/tree_borrows_lite.rs`

Single edit inside `tb_lite_check`, around the post-transition protector block (~line 1053).

**Before:**
- Protector-conflict check gated on `matches!(access, AliasAccessKind::Write)`.
- Filter required `matches!(n.kind, BorrowKind::Unique)`.
- Update predicate: `*next == TbPerm::Disabled`.

**After:**
- Gate removed: foreign reads and writes both run the protector-conflict check.
- Filter widened to `Unique | Shared`, with interior-mut exemption via the tag's existing `alias_exempt` flag:
  ```rust
  match n.kind {
      BorrowKind::Unique => true,
      BorrowKind::Shared => !tmap_for_exempt
          .get(&n.tag)
          .map_or(false, |m| m.alias_exempt),
      _ => false,
  }
  ```
- Update predicate unchanged: report only when a foreign access drives a protected node to `Disabled`.

Rationale:
- Miri TB rev of `invalidate_against_protector2` rejects a foreign write through a sibling raw while a protected `&i32` (Shared) is active. The old Unique-only filter missed it.
- The interior-mut exemption preserves pass-tests `miri_tb_pass_exact::interior_mutability` and `miri_tb_pass_exact::reserved`, where `&UnsafeCell<_>` / `&RefCell<_>` args are protected Shared but foreign-accessing the interior is deliberately not UB under TB.

### Experiments reverted (kept out of final)

Two additional changes were tried and reverted after regression analysis:

1. **Reserved{true}-on-protected widening** (report conflicted-Reserved on protected Unique at access time). Fixed `invalidate_against_protector1` but regressed pass-tests `miri_tb_pass_exact::reserved` and `miri_tb_pass_exact::interior_mutability`. Proper rule is "protected Reserved + foreign read → Frozen for non-interior-mut", which tb_lite doesn't model precisely; we'd need ReservedIM vs Reserved separation from miri TB.
2. **Ancestor-Frozen-write skip** for fresh descendant Unique (`self_node_is_fresh_unique`). Attempted to silence the `memset_u8_dynamic` FP. Caused ~5 new regressions on other TB checks (`shr_frozen_violation2`, `reservedim_spurious_write`, `subtree_traversal_skipping_diagnostics`, `interior_mutability`, `reserved`). Rolled back.

## Tests fixed by the kept change

- `miri_sb_exact::invalidate_against_protector2` — foreign write through sibling raw while `&i32` arg is protected Shared. TB tree rev rejects; now detected as `TB_LITE_PROTECTOR_CONFLICT`.
- `miri_sb_exact::invalidate_against_protector3` — previously shadowed by `MISALIGNED_ACCESS` firing first; protector-conflict now fires correctly as TB violation.

## Tests still failing (15)

### TB missed (9)

| Test | Note |
|---|---|
| `miri_sb_exact::invalidate_against_protector1` | SB-only test (lives under `miri/tests/fail/stacked_borrows/`, no tree rev). Not a TB UB; rusteze's `expected.*.tb_lite.rz` says TB violation — stale expectation. |
| `sb_lite_raw_cast_from_ref` | Rusteze self-test. Pattern: ref→raw→new foreign mut→raw access. TB doesn't UB (foreign `&mut` creation doesn't invalidate existing raw descendants absent protector). SB-semantic expectation. |
| `sb_lite_raw_from_ref_fn` | Same pattern. |
| `sb_lite_raw_read_after_unique` | Same. |
| `sb_lite_raw_transmute` | Same. |
| `sb_lite_raw_write_after_unique` | Same. |
| `sb_lite_conflict` | Overlapping shared + unique reborrow via raw. TB allows; only SB UB. |
| `ret_provenance_cases::ret_alias_parent_raw_ub` | `bounce_raw(p)` returns same pointer; two writes through aliases. No TB UB. |
| `ret_provenance_cases::ret_mut_reborrow_conflict_ub` | Callee returns `&mut *raw`; caller writes raw post-return. No protector-window conflict. No TB UB. |

All 9 encode SB-semantic expectations. Proper fix = update expected files to `ok` (per task rule "expected = miri canon"). Not applied yet pending confirmation.

### Classification drift (5)

| Test | Expected | Observed |
|---|---|---|
| `miri_sb_exact::mut_exclusive_violation2` | `TB\|READ\|RefMut\|4` | `TB\|WRITE\|RefMut\|4` |
| `miri_sb_exact::outdated_local` | `TB\|READ\|RawConst\|4` | `TB\|WRITE\|RefMut\|4` |
| `miri_tb_exact::reservedim_spurious_write` | `TB\|WRITE\|RawMut\|1` | `TB\|READ\|RefMut\|1` |
| `miri_unaligned_exact::dyn_alignment` | `MISALIGNED\|UNKNOWN\|RefShared` | `TB\|WRITE\|RefMut\|1024` |
| `tb_miri_micro::write_during_2phase` | `TB\|WRITE\|RawMut\|8` | `TB\|READ\|RefMut\|1` |

Kind matches (TB) but access and/or size flip. Root cause in instrumentation: write-assignment hook likely fires on RHS read before LHS write, so OOB/TB fires on the read path first. `dyn_alignment` is check-ordering: TB fires before misalignment; miri canon lists misalignment first.

### False positive (1)

- `memset_u8_dynamic` — rusteze fires `TB_LITE_FROZEN_WRITE` inside `for b in buf` loop. Trace shows instrumentation creates a fresh `RefMut` (tag=9) with `parent=tag=5` where tag=5 is a prior Shared Frozen tag at the same stack address. Root cause is parent-tag inheritance across stack-slot reuse in a loop iterator binding. Runtime-side narrowing proved too aggressive; needs instrumentation-side fix.

## Invariants relied on

- `tag.alias_exempt` is set by instrumentation for pointers whose pointee carries interior mutability (`UnsafeCell` and friends). If that flag stops being set correctly for such tags, the Shared-kind protector check will either FP on legitimate `&UnsafeCell` arg patterns (flag missing) or FN on genuine `&T` violations (flag over-applied).
- Transition table still maps foreign read on protected Unique Reserved{false} to `Reserved{conflicted:true}`, not `Disabled`. The widened check will therefore NOT fire on this path, matching miri TB tree semantics for non-interior-mut protected Reserved.

## Next candidates

1. Update 9 expected files for SB-semantic tests to `ok` (honest per task rule; unmasks the distinction).
2. Instrumentation: fix parent-tag inheritance on stack-slot reuse (closes `memset_u8_dynamic`).
3. Instrumentation: fix hook ordering so write-assignment reports WRITE, not prior RHS READ (closes 3 classification-drift tests).
4. Runtime: reorder TB check before MISALIGNED check in `__rz_ptr_read`/`__rz_ptr_write` — or the reverse, matching miri canonical order per test.
