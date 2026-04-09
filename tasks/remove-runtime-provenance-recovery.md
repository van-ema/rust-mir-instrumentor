# Remove Runtime Provenance Recovery

## Goal

Improve `rusteze` runtime performance by removing the **runtime same-address provenance
recovery** path from hot ref/raw creation hooks.

This task is specifically about removing:
- exact-address parent repair in the runtime
- the repair indexes that exist only to support that path
- the associated hot-path maintenance cost during tag creation

This task is **not** about removing compiler-side lineage recovery.
Compiler-side recovery is still needed and should remain in place:
- projected-place backtracking
- pointer-source recovery
- SSA lineage anchors
- return-path/source-local recovery in the instrumentation pass

Those mechanisms execute at compile time and are not the runtime performance problem.

## Motivation

Current performance notes already identify lineage repair and its indexes as part of the
remaining `ref_create` overhead:
- `tasks/bytes-rusteze-performance.md`

The current runtime still does best-effort same-address repair in several hot paths:
- `runtime/src/lib.rs`
  - `recover_parent_for_alloc_root(...)`
  - ref creation repair in `__record_ref_creation`
  - raw creation repair in `__record_raw_ptr_creation`

It also maintains repair-specific runtime state:
- `runtime/src/exact_parent_index.rs`
- `runtime/src/lineage_cache.rs`
- hot-path updates in:
  - `runtime/src/lib.rs`
  - `runtime/src/tag_pruning.rs`
  - `runtime/src/tag_history.rs`

This work exists to compensate for provenance that was not transported precisely enough by:
- compiler instrumentation
- pointer shadow
- call-boundary retagging

The long-term performance-friendly design is:
- provenance should be correct when the tag is created
- not repaired later by runtime address matching

## Non-goals

This task must **not**:
- remove `origin_base` / `origin_end` metadata if it is still needed for bounds/allocation logic
- remove compiler-side lineage recovery
- weaken soundness by inventing parent tags in the compiler pass
- silently regress current examples just to improve speed

## Current Runtime Recovery Sites

Primary function:
- `runtime/src/lib.rs`
  - `recover_parent_for_alloc_root(...)`

Ref creation sites:
- `runtime/src/lib.rs`
  - same-address repair when `resolved_parent_tag == 0`
  - root-raw parent replacement when a non-root same-address parent exists

Raw creation sites:
- `runtime/src/lib.rs`
  - projected-raw repair from `derived_from = 0`
  - parent-mismatch repair for root/raw projected shapes

Supporting indexes:
- `runtime/src/exact_parent_index.rs`
- `runtime/src/lineage_cache.rs`

Hot-path maintenance currently performed on tag creation:
- `exact_parent_index::remember_non_root_tag(...)`
- `lineage_cache::remember_non_root_tag(...)`

## Why We Cannot Just Delete It Today

Runtime recovery still covers real cases where the instrumentation is not yet precise enough.
The remaining dependency buckets are:

1. Same-address ref reborrows after optimized MIR lowering
- ref creation reaches the runtime with `parent = 0`
- same-address repair reconnects the new ref to the intended borrow family

2. Wrapper / owner extraction paths
- projected raw pointers from `Box`, `NonNull`, `Pin`, enum wrappers, or helper-heavy MIR
- runtime repair prevents a detached root from being created

3. Parent-mismatch shapes
- the derived pointer reaches the runtime with a root-like or cross-allocation parent
- runtime repair reconnects it to the same-address non-root parent

4. Return-path imprecision
- caller-side `RetRoot` / coarse return lineage can still rely on same-address repair

If we remove recovery before replacing these cases with explicit provenance transport,
we will regress examples that are currently green.

## Required Replacement Strategy

The removal should happen only after the remaining dependency buckets are replaced by
first-class provenance transport.

### 1. Keep compiler-side recovery and anchors

The compiler pass should continue to recover and preserve lineage through:
- projected-place source recovery
- pointer-source backtracking
- SSA anchor synthesis
- ref-ancestor propagation
- return-path source recovery when the source is available

These are compile-time mechanisms and should become the primary source of lineage.

### 2. Keep and extend pointer shadow where needed

Memory-backed provenance should continue to be handled by:
- typed pointer slot shadow
- bytewise pointer-shadow propagation

If any remaining runtime-repair dependency is actually a memory transport issue,
that should be fixed in pointer shadow rather than kept as address repair.

### 3. Make call-boundary provenance more explicit

Any shape that still depends on `RetRoot` + same-address repair should be replaced with:
- better `RetTake` coverage
- more precise wrapper summaries
- better callee/source recovery
- explicit opaque-call contracts where needed

### 4. Replace parent-mismatch and wrapper-root cases at creation time

The runtime should receive the correct parent directly for:
- projected raw extraction
- wrapper-return pointer derivation
- owner/internal field extraction

The desired rule is:
- parent lineage is computed once, before runtime tag creation
- not reconstructed later by address matching

## Phased Plan

## Current Status

- `Phase 0` completed:
  - `RZ_DISABLE_RUNTIME_LINEAGE_REPAIR=1` bypasses `recover_parent_for_alloc_root(...)`
- `Phase 1` completed:
  - kill-switch audit is green on:
    - all `paper_examples`
    - `bytes` seed smoke run
    - `smallvec` seed smoke run
- `Phase 2` completed for the remaining known dependency:
  - `paper_examples/lineage_field_proj`
  - the missing lineage now comes from compiler-side debug-ref activation at the raw local's
    defining assignment, using carried `ref_ancestor` metadata when present
- `Phase 3` completed:
  - hot-path updates to:
    - `exact_parent_index`
    - `lineage_cache`
    are no longer performed during normal ref/raw tag creation
- runtime repair is compile-time disabled in the normal build
  - opt back in with the `runtime_lineage_repair` feature on the `runtime` crate
- `Phase 4` is still pending:
  - the runtime repair code remains present and should be deleted only after we are satisfied
    with the profiling and stability story

### Phase 0: Add an opt-in kill switch

Introduce a temporary flag such as:
- `RZ_DISABLE_RUNTIME_LINEAGE_REPAIR=1`

Behavior:
- bypass `recover_parent_for_alloc_root(...)`
- bypass exact-parent/lineage-cache repair lookups
- keep all other runtime logic unchanged

Purpose:
- classify exactly which current examples still depend on runtime recovery

This flag must remain opt-in while the work is incomplete.

### Phase 1: Dependency audit

Run with the kill switch enabled:
- full default example suite
- full interprocedural example suite
- all `paper_examples`
- selected medium/fuzz-driver targets:
  - `bytes`
  - `smallvec`

Classify every regression into one of:
- call-boundary return imprecision
- wrapper/projection extraction
- parent-mismatch root creation
- missing compiler-side source recovery
- pointer-shadow gap

The output of this phase should be a small list of concrete failing examples and the
reason each one still needs runtime repair.

### Phase 2: Replace remaining repair-dependent shapes

For each classified failure:
- fix provenance transport at the source
- add or update a regression example
- verify the example passes with runtime repair still disabled

Expected buckets:

1. Return-path fixes
- reduce `RetRoot` usage
- improve return-source recovery

2. Wrapper/projection fixes
- carry parent lineage through helper-heavy projected raw creation

3. Parent-mismatch fixes
- stop creating root-like tags when a better parent is already knowable in the compiler pass

### Phase 3: Stop maintaining repair indexes on the hot path

Once the kill-switch validation is green:
- stop updating:
  - `exact_parent_index`
  - `lineage_cache`
for normal tag creation

This is the first performance win that should show up directly in profiling.

### Phase 4: Remove the runtime repair implementation

Delete:
- `recover_parent_for_alloc_root(...)`
- exact-parent repair branches in ref/raw creation
- `runtime/src/exact_parent_index.rs`
- runtime lineage-cache repair support if no longer needed elsewhere

Keep only metadata that is still used for other purposes:
- origin metadata
- allocation epoch snapshotting
- bounds/origin checks unrelated to lineage repair

## Validation

Before each commit:
- `CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py`
- `RZ_INTERPROC_UNSAFE_SUMMARIES=1 CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py`

Also run targeted regression checks with the kill switch enabled:
- all `paper_examples`
- especially lineage-sensitive ones

Recommended targeted set:
- `paper_examples/lineage_opt_away`
- `paper_examples/lineage_cfg_join`
- `paper_examples/lineage_cfg_branch_join`
- `paper_examples/lineage_loop_carried`
- `paper_examples/lineage_helper_nested`
- `paper_examples/lineage_mixed_mem_ssa`
- `paper_examples/lineage_field_proj`
- `paper_examples/lineage_memcpy_roundtrip`
- `paper_examples/lineage_static_smuggle`
- `paper_examples/tb_aliasing_mut3`
- `paper_examples/tb_shr_frozen_violation1`

If a target regresses with runtime repair disabled, do not remove the fallback yet.

## Success Criteria

1. With runtime recovery disabled, all current example suites remain green.
2. The important lineage-sensitive `paper_examples` still reproduce their intended behavior.
3. The runtime no longer performs exact-parent same-address repair on tag creation.
4. Repair-only runtime indexes are no longer maintained on the hot path.
5. Hook profiling shows a measurable reduction in:
- `ref_create_lineage_repair_ns`
- `ref_create_exact_parent_update_ns`
- `ref_create_lineage_cache_update_ns`

## Expected Benefit

The main expected performance gain is lower overhead in:
- `__record_ref_creation`
- `__record_raw_ptr_creation`

This should reduce:
- hash-map lookups for same-address repair
- index maintenance for future repair
- part of the residual `ref_create` cost in fuzzing workloads

The desired end state is simpler:
- compiler pass computes lineage
- pointer shadow transports lineage through memory
- runtime checks lineage
- runtime does not reconstruct lineage by address matching
