# Tree Borrows Redesign Plan

## Goal

Reduce reliance on TB-lite recovery rules in `runtime/src/alias_model/tree_borrows_lite.rs`
by making the instrumented lineage and state transitions match the Tree Borrows model
more directly.

The target is not to remove every fallback immediately. The target is:

1. helper reads/writes are attached to the right family in instrumentation
2. `Reserved -> Active/Frozen/Disabled` transitions reflect local-vs-foreign access
3. call/return boundaries transport the parent Miri would retag from
4. recovery rules become dead or rare, and can then be removed with confidence

## Current problems

Today TB-lite compensates for several instrumentation/runtime mismatches with ad hoc
recovery predicates such as:

- `tb_same_lineage_frozen_raw_const_write_ok`
- `tb_has_usable_clean_unique_ancestor`
- `tb_only_unique_ancestor_overlap`
- `tb_lite_reactivatable_same_lineage`
- same-family frozen-write allowances in the `TbPerm::Frozen` write branch

Those rules are papering over three distinct problems:

1. **Wrong parent selection**
   - helper borrows/views sometimes derive from an older raw/carrier/anchor family
     instead of the current exact receiver family
   - result: accesses that should be local become foreign and freeze the mutable family

2. **Boundary parent loss**
   - exact local tag and "parent for next `FnEntry` retag" are not always the same
   - recovered refs/raws can be exported through the wrong tag

3. **Compressed overlap modeling**
   - TB-lite stores coarse primary ranges plus `extra_ranges`
   - some recovery predicates still reason only over the primary range and/or only over
     `Unique`, which makes them incomplete for legitimate same-family accesses

## Target semantics

We should approximate Miri's Tree Borrows behavior, not repair it after the fact.

For a mutable family:

1. `&mut` starts in `Reserved`
2. local descendant reads are allowed before first write
3. the first local write activates the family
4. only foreign conflicting accesses should freeze or disable it

For shared/raw helper views derived from an active receiver:

- if they are semantically part of the current receiver borrow, they must stay under the
  current receiver family
- they must not be re-rooted through an older carrier/raw lineage just because MIR happens
  to go through a field/projection

For function boundaries:

- the caller must export the parent the callee should retag from
- the callee should not need runtime canonicalization except as a fallback

## Required implementation changes

### 1. Make borrow context explicit in instrumentation

Parent selection is currently too heuristic-heavy in:

- `parent_tag_operand_for_src_place`
- `recover_pointer_source_local_for_projected_place`
- related backtracking helpers in `instrument-mir/src/instrumentation.rs`

Add an explicit notion of **borrow context**:

```rust
enum ParentSelectionMode {
    ReceiverFamily,
    PointeeFamily,
}
```

Use it to distinguish:

- **ReceiverFamily**
  - helper refs/raws/views that are semantically still part of the current `&self`/`&mut self`
  - prefer the current exact receiver tag
  - do not backtrack to an older raw/carrier/anchor parent unless the path actually leaves the
    receiver family

- **PointeeFamily**
  - nested pointer extraction
  - returned/recovered pointer values
  - explicit pointer-field lineage continuity
  - keep current pointer-source recovery here

If ambiguous, keep the current conservative behavior.

### 2. Treat call/return export-parent as first-class state

The current explicit export-parent path is the right direction, but it is still incomplete.

Make these channels authoritative:

- exact local tag
- export parent
- export-parent recovered bit

Requirements:

- propagate export-parent through `Ref`/copy/aggregate forwarding where the value is still the
  same boundary-recovered family
- drive both `CallArgPush` and `RetPush` from export-parent state, not from older heuristics
- preserve export-parent through shadowed storage/reload where refs are stored and later reloaded

### 3. Introduce explicit slot-family state for non-pointer carriers

For stack locals like `BytesMut`, `SmallVec`, etc., distinguish:

- current exact temp-local tag for helper refs
- current slot-family tag for future borrows of the underlying stack slot

Requirements:

- a whole-place helper `&self` borrow must not overwrite the mutable slot-family identity
- a later `&mut self` borrow must continue from the current slot family
- `MutArgRetTake` writeback should update the slot-family/export-parent, not just a transient
  temp-local exact tag

### 4. Make TB-lite recovery predicates reason over the actual overlap model

As long as TB-lite keeps `extra_ranges`, every same-family overlap predicate must use
`tb_node_overlaps`, not just the primary range, and must not special-case `RawMut` where the
same logic should apply to `RefMut`.

This is still a runtime change, but it should be expressed as part of the main transition
logic, not as target-specific escapes.

### 5. Shrink the runtime "hatches" only after instrumentation is corrected

Do not delete all helpers up front.

Instead:

1. add temporary counters/trace logging for each recovery predicate
2. run:
   - full example suite
   - full interproc suite
   - current `bytes` bucket
   - current `smallvec` bucket
3. identify which helpers still fire
4. remove dead helpers one at a time

## Suggested implementation order

### Phase 1: Parent-selection cleanup

1. Add `ParentSelectionMode`
2. Convert receiver-derived helper sites to `ReceiverFamily`
3. Keep nested pointer extraction on `PointeeFamily`
4. Re-run all gates

Expected effect:
- fewer false freezes from helper reads like `self.len()` / `self.capacity()`

### Phase 2: Boundary parent completion

1. Complete export-parent propagation across refs/copies/returns
2. Teach return export to use export-parent symmetrically with call arguments
3. Re-run all gates

Expected effect:
- fewer `CALL_ARG` / `RET invalid ref` false positives

### Phase 3: Slot-family cleanup

1. Separate temp exact tag from slot-family identity for non-pointer carriers
2. Update `MutArgRetTake` / direct borrow creation / local writeback paths
3. Re-run all gates

Expected effect:
- fewer root-ref-create / wrong-family reborrow false positives

### Phase 4: Runtime simplification

1. Add usage counters for current recovery predicates
2. Remove dead predicates
3. Generalize any remaining ones into one coherent same-family/local transition rule

Expected effect:
- smaller TB-lite state machine
- clearer correspondence to the paper's local-vs-foreign model

## Validation requirements

Any phase that touches `instrument-mir/` or `runtime/` must pass:

- `CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py`
- `RZ_INTERPROC_UNSAFE_SUMMARIES=1 CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py`

And for this redesign specifically, also replay:

- current `bytes` crash bucket
- current `smallvec` crash bucket

## Non-goals

- Do not try to make TB-lite identical to Miri's implementation details
- Do not remove strict checks for true foreign overlap or protector conflicts
- Do not replace principled parent-selection fixes with more target-specific exceptions
