# Bytes Fuzzing Performance Notes

## Context

Target:
- `afl_harness/src/bin/afl_bytes_driver.rs`

Observed issue:
- `rusteze` debug fuzzing for `bytes` was much slower than ASan and hit AFL timeouts.

The original harness amplified runtime cost because it:
- kept a parallel oracle model with `Vec<u8>`
- ran deep semantic checks after every step
- repeatedly forced extra `clone` / `freeze` / `try_into_mut` / rebuild paths

That harness has now been rewritten into a fast no-oracle version because, for these fuzz
campaigns, we care about:
- memory-safety bugs
- alias-model violations

not semantic mismatches.

## Measurements

### Old harness

On a stress input:
- `rusteze debug tb_lite`: about `0.58s`
- `rusteze debug none`: about `0.09s`
- `ASan debug`: below shell timer resolution

Hook profile on the stress input showed the dominant costs were:
- `ref_create`
- `read alias_check`
- `write alias_check`

Representative profile:
- `ref_create total`: `305.9 ms`
- `read alias_check`: `177.1 ms`
- `write alias_check`: `33.5 ms`

### Current no-oracle harness

Current seed timings:
- `rusteze debug tb_lite`: avg `3.68 ms`
- `rusteze debug none`: avg `2.53 ms`
- `ASan debug`: avg `6.98 ms`

Stress input:
- `rusteze debug tb_lite`: about `0.24s`
- `rusteze debug none`: about `0.01s`
- `ASan debug`: about `0.01s`

The harness overhead multiplier is gone. The remaining slowdown is mostly real `rusteze`
runtime work.

## Remaining runtime hotspots

### 1. `ref_create`

Main code:
- `runtime/src/lib.rs:3102`

Most expensive sub-steps:
- `active_alias_model().on_tag_created(...)`
- tag-store / index updates
- allocation snapshot / lineage repair

Relevant lines:
- `runtime/src/lib.rs:3150`
- `runtime/src/lib.rs:3245`
- `runtime/src/lib.rs:3347`
- `runtime/src/lib.rs:3356`
- `runtime/src/lib.rs:3361`
- `runtime/src/lib.rs:3365`

High-value optimizations:

1. Lazy TB materialization for ordinary `RefShared`
- In `tb_lite`, not every shared ref needs an eager TB node.
- Keep full `TagMeta`, but skip `tb_lite_on_tag_created` for non-protected `RefShared`.
- Materialize the TB node on first access instead.
- Keep eager creation for:
  - `RefMut`
  - protected refs
  - raw tags if required by the current model

2. Narrow index maintenance
- Do not update every repair/index structure for every new ref.
- Restrict:
  - `remember_alloc_epoch_tag`
  - `tag_pruning::remember_live_tag`
  - `exact_parent_index::remember_non_root_tag`
  - `lineage_cache::remember_non_root_tag`
- Prefer indexing only:
  - non-root tags
  - mutable-capable tags
  - tags actually needed by lineage repair

3. Make `tag_store::insert` populate shards directly
- `runtime/src/tag_store.rs:124`
- Avoid the later global-map fallback and shard backfill on first lookup.

4. Add a stricter fast path before lineage repair
- If `parent_tag` already has:
  - same pointee
  - same epoch
  - compatible bounds
- skip the repair search.

## 2. `read alias_check` / `write alias_check`

Main code:
- `runtime/src/alias_model/tree_borrows_lite.rs:535`

Current issue:
- `tb_lite_check` scans all overlapping live nodes in the allocation on every access.
- Child/foreign classification repeatedly walks ancestry.

High-value optimizations:

1. Singleton fast path
- If an allocation/epoch has only one live node and it is the access tag, return immediately.

2. Overlap index
- Replace full `tree.nodes.values()` overlap scans with an interval structure or sorted live-range list.
- Query only nodes that may overlap `[addr, addr + size)`.

3. Precompute access ancestry once
- Build the ancestor chain of `access_tag` once per access.
- Use that to classify child/foreign in O(1) instead of repeated parent walks.

4. Fold protector detection into the main scan
- Avoid the second scan for protected overlap on writes.

5. Cache raw-tag nearest ref ancestor
- `__rz_ptr_read` / `__rz_ptr_write` currently recover the ref ancestor on the hot path for raw tags.
- Cache it in tag metadata or a small side cache.

## Recommended implementation order

1. Lazy-materialize unprotected `RefShared` TB nodes
2. Add singleton fast path in `tb_lite_check`
3. Precompute `access_tag` ancestry once per check
4. Narrow exact-parent / lineage-cache updates
5. Make `tag_store::insert` populate shards directly

This order should cut the two biggest remaining costs:
- `ref_create`
- alias checks on reads/writes

## TODO

- [x] Replace the old oracle-heavy `bytes` harness with the current fast no-oracle harness.

- [x] Re-measure the no-oracle harness and confirm the harness-level overhead multiplier is gone.

- [ ] Lazy-materialize ordinary unprotected `RefShared` TB nodes.
  Keep eager materialization for `RefMut`, protected refs, and raw tags that the current model
  still requires.

- [ ] Add a singleton fast path in `tb_lite_check`.
  If an allocation/epoch has only the access tag live, return before the full overlap walk.

- [ ] Precompute the access ancestry once per TB-lite check.
  Use that cached ancestry for child/foreign classification instead of repeated parent walks.

- [ ] Narrow exact-parent and lineage-cache updates.
  Index only tags that repair and lineage lookup actually need.

- [ ] Make `tag_store::insert` populate shards directly.
  Avoid first-lookup shard backfill on hot paths.

- [ ] Re-profile `bytes` after each optimization.
  Keep the full example gates green and compare `ref_create`, read alias-check, and write
  alias-check timings against the measurements above.
