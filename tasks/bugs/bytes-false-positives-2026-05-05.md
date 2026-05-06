# Bytes False Positives (2026-05-05)

## Status

Update after:

- `d76966c` `Preserve reversible exposed provenance`
- `b987c03` `Allow RefMut same-family frozen writes`
- receiver-family parent selection for projected carrier helper accesses

Current crash bucket:

- `fuzz/out/bytes/default/crashes/id:000000..000067`

Classification on fresh binaries:

- instrumented harness: all current files exit `0`
- plain harness: all current files exit `0`
- the old `FROZEN_WRITE` cluster is fixed
- the remaining stack-carrier field false positives are fixed
- all files in the current bucket are stale on the current instrumented build

So there is no remaining live `bytes` false-positive in the current saved bucket on this build.

## Live crash classes

### 1. `WILD_POINTER` during `Bytes::clone`

Representative file:

- `id:000020,sig:06,src:000004,time:219681,execs:32300,op:havoc,rep:2`

Representative violation:

- `WILD_POINTER`
- `READ via raw derive addr=... size=1`
- `reason=NO_PROVENANCE_DERIVE kind=RawMut parent=0`
- backtrace:
  - `bytes::bytes::shallow_clone_vec`
  - `bytes::bytes::promotable_even_clone`
  - `<bytes::bytes::Bytes as Clone>::clone`

Trigger shape:

- short sequence:
  - build `BytesMut`
  - `freeze`
  - `clone`
  - later `slice_ref`

Root cause:

- `bytes` uses pointer tagging / untagging through `ptr_map(...)`:
  - store low-bit metadata in a pointer-typed field
  - later recover the original pointer by masking the low bit back out
- on Miri, `ptr_map` preserves provenance by using `wrapping_add(diff)`
- on the native `bytes` build, the same helper becomes integer-style address rewriting:
  - `new_addr as *mut u8`
- rusteze currently sees the recovered pointer as a root/no-provenance raw creation
  instead of a same-lineage raw derivation from the original tracked pointer

Why this is a false positive:

- `bytes` is intentionally doing reversible pointer tagging here
- the resulting pointer still names the same allocation base
- the target program is not dereferencing a forged integer pointer from nowhere; it is recovering
  a previously tagged pointer

Principled patch:

1. Preserve raw ancestry across reversible same-base pointer-tagging transforms
   - when a raw pointer is reconstructed from a known pointer source by a same-base address map
     such as `ptr_map(shared, |addr| addr & !KIND_MASK)`, carry the source raw family forward
     instead of treating the result as exposed-provenance root
2. Keep `without_provenance(...)` strict
   - this patch should be narrow to reversible pointer-tagging / untagging
   - it should not relax genuine integer-to-pointer forgery

Relevant code paths:

- `third_party/bytes/src/bytes.rs`
  - `ptr_map`
  - `promotable_even_clone`
  - `shallow_clone_vec`
- rusteze raw-creation plumbing:
  - `instrument-mir/src/instrumentation.rs`
  - `runtime/src/lib.rs`

Resolved by `d76966c`.

### 2. `FROZEN_WRITE` in `BytesMut::reserve_inner`

Representative files:

- `id:000049`
- `id:000051`
- `id:000053`
- `id:000054`
- `id:000055`
- `id:000058`
- `id:000059`
- `id:000066`

Representative violation:

- `TREE_BORROWS_VIOLATION`
- `WRITE via tag=... kind=RefMut`
- `reason=TB_LITE_FROZEN_WRITE`
- backtrace:
  - `bytes::bytes_mut::BytesMut::reserve_inner`
  - `bytes::bytes_mut::BytesMut::reserve`

Observed TB-lite shape:

- the failing write goes through a fresh `Unique` child
- its nearest same-address `Unique` ancestor is still live but already `Frozen`
- the helper-created `Shared` descendants under that same lineage are already dead / disabled
- there is no evidence of a foreign live overlapping lineage in the representative dump

Representative dump shape:

```text
302  Unique  Frozen  alive=true    range=[whole BytesMut slot]
└─593 Unique  Reserved alive=true  range=[same slot]

589,590 Shared descendants under 302 are already dead/disabled
```

Interpretation:

- temporary same-lineage helper borrows such as `self.len()` / `self.capacity()` freeze the
  current `&mut self` family
- later writes in the same `reserve_inner` method happen through a descendant `Unique`
- TB-lite still treats the frozen ancestor as blocking the write even after the helper
  descendants are dead

Why this is a false positive:

- these are ordinary `bytes` growth paths
- plain execution succeeds
- Miri-style temporary helper borrows should not permanently block later same-lineage mutation
  once those helper borrows are gone

Principled patch:

1. Extend TB-lite's same-lineage write recovery for `RefMut`
2. Allow a descendant `Unique` write under a frozen `Unique` ancestor when:
   - all overlapping blockers are within the same lineage
   - the helper descendants that caused the freeze are already dead / disabled
   - there is no foreign live overlapping lineage
   - there is no protector conflict
3. Keep cross-lineage and live-foreign overlap strict

This is a runtime alias-model patch, not a broad instrumentation relaxation.

Relevant code paths:

- `runtime/src/alias_model/tree_borrows_lite.rs`
  - frozen-write handling
  - same-lineage reactivation helpers
  - `tb_has_usable_clean_unique_ancestor`
  - `tb_only_unique_ancestor_overlap`

Resolved by `b987c03`.

### 3. `FROZEN_WRITE` in `spare_capacity_fill`

Representative files:

- `id:000057`
- `id:000065`

Representative violation:

- `TREE_BORROWS_VIOLATION`
- `WRITE via tag=... kind=RefMut`
- `reason=TB_LITE_FROZEN_WRITE`
- backtrace stops in `afl_bytes_driver::run_step`
- the failing operation is the harness's `spare_capacity_fill` branch:
  - `buf.reserve(additional)`
  - iterate `buf.spare_capacity_mut().iter_mut().take(write_len)`
  - `cell.write(fill)`

Observed TB-lite shape:

- the failing per-byte `Unique` write has live same-lineage `RawConst` siblings on the exact
  written byte
- those `RawConst` siblings were created by helper / iteration internals, not by a foreign alias

Representative dump shape:

```text
852 Unique Reserved alive=true range=[spare slice region]
├─854 RawConst Frozen alive=true range=[exact byte]
└─860 Unique Reserved alive=true range=[exact byte]   <-- failing write
```

Interpretation:

- rusteze is treating temporary same-lineage read-only helper views over spare capacity as
  blockers for the subsequent `MaybeUninit::write`
- this is the byte-granular analogue of the `reserve_inner` failure

Why this is a false positive:

- the write is an ordinary fill into spare capacity
- plain execution succeeds
- there is no evidence of a foreign live alias on the written byte in the representative dump

Principled patch:

1. Generalize the same-lineage frozen-raw-const allowance so it also covers helper-created
   same-lineage sibling `RawConst` blockers for `RefMut` writes
2. Keep the rule narrow:
   - same lineage only
   - overlapping byte range only
   - no foreign live overlap
   - no protector conflict

This belongs in the TB-lite runtime, alongside the existing same-lineage raw-const logic.

Relevant code paths:

- `runtime/src/alias_model/tree_borrows_lite.rs`
  - `tb_same_lineage_frozen_raw_const_write_ok`
  - frozen-write handling for `RefMut`

Resolved by `b987c03`.

## Remaining live classes

### A. Dead stack-field ref-create in `promotable_even_drop`

Representative file:

- `id:000021,sig:06,src:000004,time:219823,execs:32304,op:havoc,rep:2`

Representative violation:

- `USE_AFTER_DEAD`
- `READ via root ref create`
- `reason=REF_CREATE_FROM_DEAD_ALLOC`
- backtrace:
  - `bytes::bytes::promotable_even_drop`
  - `core::ptr::drop_in_place<bytes::bytes::Bytes>`

Observed runtime evidence:

```text
[tag-create][raw] tag=362 parent=0 resolved_parent=0 pointee=0x...3730 kind=const bounds=0 align=1 epoch=1
...
USE_AFTER_DEAD
READ via root ref create addr=0x7fff...7938 size=8
reason=REF_CREATE_FROM_DEAD_ALLOC ... kind=RefShared parent=362
```

Interpretation:

- a helper ref creation over a stack field in drop glue is being parented from the heap payload
  raw tag (`362`)
- the actual pointee address is a stack field inside the dropping `Bytes` carrier
- because the parent is from the heap payload family instead of the current carrier/receiver
  family, ref creation is treated as a root stack read against a dead stack alloc

This is a parent-selection bug, not a real UAD.

### B. Misaligned carrier-field reads in `Bytes::slice_ref`

Representative files:

- `id:000057,sig:06,src:000238,time:1337010,execs:107453,op:havoc,rep:13`
- `id:000067,sig:06,src:000354,time:3945009,execs:211872,op:havoc,rep:13`

Representative violation:

- `MISALIGNED_ACCESS`
- `READ via tag=499 ... kind=RawMut parent=0 pointee=0x...`
- backtrace:
  - `bytes::bytes::Bytes::slice`
  - `bytes::bytes::Bytes::slice_ref`

Observed runtime evidence:

```text
[tag-create][raw] tag=499 parent=0 resolved_parent=0 pointee=0x557f...27c0 kind=mut bounds=0 align=1 epoch=1
...
MISALIGNED_ACCESS
READ via tag=499 addr=0x7ffd...b340 size=8
required_alignment=8 guaranteed_alignment=1
```

Interpretation:

- the read is on a stack `Bytes` object field (`self.len`, `self.ptr`, etc.)
- the access tag is a heap-origin raw tag with alignment `1`
- instrumentation is reusing the payload/raw family as the parent for helper field reads on the
  `Bytes` carrier
- runtime then applies heap-payload alignment to a stack-carrier field read and reports a bogus
  misalignment

### C. Carrier-field OOB read in `Bytes::slice_ref`

Representative file:

- `id:000060,sig:06,src:000371,time:2677087,execs:161863,op:havoc,rep:4`

Representative violation:

- `OUT_OF_BOUNDS`
- `READ via tag=1815`
- `origin_alloc_base=0x... origin_alloc_size=150`
- backtrace:
  - `bytes::bytes::Bytes::slice`
  - `bytes::bytes::Bytes::slice_ref`

Observed runtime evidence:

```text
[tag-create][raw] tag=1814 parent=1812 resolved_parent=1812 pointee=0x55fa...0d30 kind=mut bounds=0 align=1 epoch=1
[tag-create][raw] tag=1815 parent=0 resolved_parent=1814 pointee=0x55fa...0d30 kind=mut bounds=150 align=0 epoch=1 hint=198
...
OUT_OF_BOUNDS
READ via tag=1815 addr=0x7ffc...0e80 size=8
(no containing alloc for addr, but tag derives from alloc)
```

Interpretation:

- same root cause as the misalignment class
- a heap-origin raw tag with payload bounds is being used for a stack-carrier field read
- runtime compares the stack field read against heap payload bounds and reports bogus OOB

## Unified root cause for the remaining classes

The remaining live classes are all caused by the same design bug:

> projected helper refs/raws on a non-pointer carrier (`Bytes`) are still being parented from the
> nested payload/raw family instead of the current receiver/carrier family.

This is the exact `ReceiverFamily` vs `PointeeFamily` split called out in
`tasks/tree-borrows-principled-redesign.md`.

In these crashes:

- the carrier object lives on the stack
- the nested payload pointer lives on the heap
- instrumentation chooses the heap payload lineage when it should use the current stack-carrier
  borrow family

That mis-parenting then manifests as:

- dead stack field read (`id:000021`)
- misaligned stack field read with heap alignment metadata (`id:000057`, `id:000067`)
- stack field read checked against heap payload bounds (`id:000060`)

## Proposed patch

Patch instrumentation, not runtime.

### 1. Add explicit borrow context

Introduce a parent-selection mode in `instrument-mir/src/instrumentation.rs`:

```rust
enum ParentSelectionMode {
    ReceiverFamily,
    PointeeFamily,
}
```

### 2. Use `ReceiverFamily` for carrier helper refs/raws

For projected helper accesses that remain inside the current non-pointer carrier object:

- field refs in drop glue such as `&mut self.data`
- helper field reads inside methods such as `self.len`, `self.ptr`, `self.vtable`
- local helper raw views used only to inspect the carrier object

prefer the current exact receiver/carrier tag over:

- recovered nested payload pointers
- pointer-carrier backtracking
- projectionless slot-anchor fallback

Concretely, in `parent_tag_operand_for_src_place`:

- if the base backtracks to the current receiver/carrier local
- and the access stays within the carrier object graph
- and we are not explicitly extracting a nested pointer value to escape

then return:

- `tag_local_for_ptr_local[receiver]` first
- otherwise `ref_ancestor_local_for_ptr_local[receiver]`

and skip the pointer-source recovery heuristics for that site.

### 3. Keep `PointeeFamily` only for true nested pointer extraction

Retain the current pointer-source recovery path for:

- `self.ptr`
- returned slices / returned raw pointers
- nested pointer fields that are escaping as their own pointer values

This keeps the already-fixed boundary/raw provenance work intact.

### 4. Likely implementation sites

- `parent_tag_operand_for_src_place`
- `recover_pointer_source_local_for_projected_place`
- callers that currently materialize helper ref/raw parents for:
  - `InstrKind::Ref`
  - raw loads/casts
  - projected reborrow parent locals

## Recommended next step

Implement the `ReceiverFamily` fast path first and replay:

- `id:000021`
- `id:000057`
- `id:000060`
- `id:000067`

If that diagnosis is right, those four crashes should all disappear together, because they are
the same bug expressed through different runtime checks.

## Recommended patch order

1. Fix receiver-family vs pointee-family parent selection for carrier helper refs/raws
2. Replay the live `bytes` crash bucket on the instrumented and plain harnesses
3. Run the full example suite in both modes before committing any instrumentation/runtime change

## Notes

- The remaining four crashes are not independent target bugs.
- They are one instrumentation bug: carrier helper accesses are using payload lineage.
