# SmallVec False Positives (2026-05-05)

## Status

Current crash bucket:

- `fuzz/out/smallvec/default/crashes/id:000000..000010`

Classification:

- instrumented harness: all crash
- plain harness: all exit `0`

So the current `smallvec` bucket is false-positive-only.

## Crash classes

### 1. Return-boundary shared-ref false positive

Representative file:

- `id:000000,sig:06,src:000000,time:202,execs:81,op:arith8,pos:0,val:+5`

Representative violation:

- `TREE_BORROWS_VIOLATION`
- `RET invalid ref tag=20 pointee=... kind=RefShared`
- `reason=TB_LITE_INVALIDATED`
- raised in `__rz_push_ret_tag`

Trigger shape:

- the harness input is one byte: `05`
- this reaches only the read-only branch:
  - `let sum: u32 = v.iter().map(|&x| x as u32).sum();`

Interpretation:

- a helper-heavy returned shared ref is being exported at a function return boundary
- instrumentation still exports the returned local's exact shared child tag
- runtime validates that exact shared tag before any return-side repair
- the exact child is already invalid, even though the live same-address family still exists

Root cause:

- `RetPush` still exports `tag_local_for_ptr_local[ptr_local]`
- it does not use the already-maintained `(export_parent, export_parent_is_recovered)` channel
- this is the return-side analogue of the call-argument bug fixed for `bytes`

Principled patch:

1. Make `RetPush` use the same parent-selection model as `CallArgPush`
   - if `export_parent_is_recovered == 1`, export `export_parent`
   - otherwise export the exact local tag
2. Keep runtime strict
   - `__rz_push_ret_tag` can continue exact-first validation for direct returns
   - the instrumentation must export the right boundary parent for recovered shared returns

Patch site:

- `instrument-mir/src/instrumentation.rs`, `InstrKind::RetPush` lowering

### 2. Whole-slot mutable-write false positive (`SmallVec::set_len`)

Representative files:

- `id:000001..000010`

Representative violation:

- `TREE_BORROWS_VIOLATION`
- `WRITE via tag=... kind=RefMut`
- `reason=TB_LITE_FROZEN_WRITE`
- backtrace ends in `smallvec::SmallVec<T,_>::set_len`

Representative trigger:

- ordinary short mutation sequences like:
  - `push`
  - `pop`
  - `push`
  - `insert`

Observed TB-lite shape:

- a live whole-slot `Shared/Frozen` ancestor remains over the `SmallVec` stack slot
- later `&mut self` write tags descend under that shared family
- `set_len` then writes through `RefMut` under a frozen shared ancestor
- runtime correctly reports `TB_LITE_FROZEN_WRITE` for the metadata it received

Interpretation:

- this is not a runtime dead-temp problem
- it is a whole-place parent-selection problem for non-pointer carrier locals such as `SmallVec`

Likely source pattern:

- helper reads such as `v.len()` create temporary whole-place shared borrows of `v`
- later `v.insert(...)` creates `&mut v`
- the mutable borrow is being derived under the shared family rather than the local's slot-family anchor

Root cause:

- projectionless whole-place borrows of non-pointer stack locals still allow temporary shared borrows
  to become the governing family for the slot
- the later mutable borrow does not reliably reattach to the stable slot-family anchor

Principled patch:

1. Treat the stack-slot family as authoritative for future whole-place `&mut local` borrows of
   interesting non-pointer locals such as `SmallVec`
2. Do not allow transient whole-place shared helper borrows (`&v` for `len()`, iteration setup, etc.)
   to overwrite that slot-family identity
3. Keep exact local tags for those shared helper borrows for local checking, but derive future
   mutable borrows from the slot-family anchor rather than from the latest shared exact tag

Relevant code paths:

- `instrument-mir/src/instrumentation.rs`
  - `parent_tag_operand_for_src_place`
  - `materialize_projectionless_slot_anchor_parent_local`
  - `InstrKind::Ref` parent selection

## Notes

- These two classes are independent.
- The first is a return-boundary parent-export bug.
- The second is a whole-slot borrow-family selection bug.
