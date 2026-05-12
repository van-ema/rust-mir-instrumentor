# Bytes fuzz crashes on 2026-05-12

## Status

- Replayed the current `fuzz/out/bytes/default/crashes` bucket on the current instrumented harness.
- Representative inputs pass under the plain harness and under Miri Tree Borrows.
- The live crashes are false positives from rusteze modeling gaps, not `bytes` memory bugs.
- Implemented the first-family fix: direct-ref by-value return carriers now export a return anchor
  (`RetAnchorPush`) in addition to leaf shadow.

## Current crash families

1. `Bytes::split_off` / `Bytes::split_to`
   - `TREE_BORROWS_VIOLATION`
   - root cause: by-value `Bytes` return carriers lose their return anchor, so later borrows of the
     direct-ref `vtable` field root at `parent=0`.

2. `Bytes::slice`
   - `OUT_OF_BOUNDS` / `MISALIGNED_ACCESS`
   - root cause: helper-local stack/range-carrier family corruption.

3. `BytesMut::spare_capacity_mut`
   - `WILD_POINTER`
   - root cause: fresh/empty path lineage loss before `from_raw_parts_mut`.

4. `BytesMut::freeze` / `extend_from_slice`
   - `WILD_POINTER`
   - root cause: reversible pointer-tagging provenance loss.

## First family: `split_off` / `split_to`

Representative safe source shape:

```rust
let mut b = bytes::Bytes::from_static(b"abcdef");
let tail = b.split_off(1);
```

`bytes` implementation:

```rust
pub fn split_off(&mut self, at: usize) -> Self {
    let mut ret = self.clone();
    self.len = at;
    unsafe { ret.inc_start(at) };
    ret
}
```

### What should happen

Semantically this is:

1. `self: &mut Bytes` is the active unique family.
2. `self.clone()` creates a second `Bytes` value that shares the same backing storage.
3. `self.len = at` mutates only the stack carrier metadata of `self`.
4. `ret.inc_start(at)` mutates only the stack carrier metadata of `ret`.

Under Miri / Tree Borrows, helper reads of `self.vtable` during `clone()` stay under the live
receiver lineage. They do not become foreign root borrows of the stack carrier.

### What rusteze does now

The bad tag is a root shared borrow of the stack carrier's `vtable` field:

- `tag=466 parent=0 kind=Shared range=[carrier.vtable_slot]`
- later write: `WRITE via tag=457 ... reason=TB_LITE_2PHASE_CONFLICT tag=449`

From the runtime trace:

```text
[rusteze-runtime][tag-create][ref] tag=466 parent=0 resolved_parent=0
  pointee=0x...dec8 kind=shared bounds=8 epoch=0

[tb-trace] access=Read orig_tag=466 access_tag=466 addr=0x...dec8 size=1 kind=RefShared
[tb-trace]   node tag=466 parent=0 kind=Shared perm=Frozen ...

TREE_BORROWS_VIOLATION
WRITE via tag=457 addr=0x...dec0 size=8 kind=RefMut
reason=TB_LITE_2PHASE_CONFLICT tag=449
```

The addresses line up with the `Bytes` stack carrier:

- `addr=...dec0` is the `len` field write in `self.len = at`
- `addr=...dec8` is the trailing `vtable: &'static Vtable` field slot

So the violation is not about shared backing storage. It is a false foreign read of the stack
carrier metadata.

### Why the root shared tag appears

`Bytes` uses the by-value ref-carrier path because it has a direct ref field:

```rust
pub struct Bytes {
    ptr: *const u8,
    len: usize,
    data: AtomicPtr<()>,
    vtable: &'static Vtable,
}
```

Caller side is already structured to import a return anchor for such values:

- caller inserts `RetAnchorTake`
- caller also restores pointer-leaf shadow with `RetLeafTake`

But callee side only exports leaf shadow on `Return`. It does **not** export the matching return
anchor tag. As a result:

1. `clone()` returns a by-value `Bytes`
2. caller executes `RetAnchorTake`
3. `__rz_take_ret_tag(callee_id, 0)` returns `0` because nothing pushed the anchor
4. the destination carrier anchor becomes `0`
5. the next borrow of `ret.vtable` becomes a fresh root shared tag on the stack slot

That is the structural gap. The caller-side design exists; the callee-side export is missing.

### Principled patch

Add explicit return-anchor export for non-pointer direct-ref carriers.

Design:

1. On `Return`, if `RETURN_PLACE` satisfies `supports_call_boundary_anchor_local(...)`, emit a new
   callee-side `RetAnchorPush`.
2. `RetAnchorPush` should export the carrier anchor local with `__rz_push_ret_tag(callee_id, 0, anchor)`.
   - use `addr=0` to match the existing `RetAnchorTake` lookup key
   - do not synthesize a separate field borrow or helper-specific repair
3. Keep existing `RetLeafPush` for the returned pointer leaves.
4. Keep existing caller-side `RetAnchorTake` / `RetLeafTake` unchanged.

This is aligned with the current call-boundary design:

- leaf shadow transports exact nested pointer state
- anchor transport carries the outer slot family for by-value ref carriers

It is also aligned with the Miri / TB model:

- the returned `Bytes` carrier stays in the same family exported by the callee
- later borrows of `ret.vtable` become descendants of that imported family
- no fresh `parent=0` helper authority is created

### Expected post-patch behavior on the example

Before:

```text
clone return anchor not exported
=> caller RetAnchorTake gets 0
=> borrow ret.vtable creates root Shared(parent=0)
=> later self.len write conflicts
```

After:

```text
clone return anchor exported
=> caller RetAnchorTake imports the live family
=> borrow ret.vtable creates Shared(child of imported family)
=> later self.len write is still local and remains allowed
```

### Implemented patch

Implemented in `instrument-mir/src/instrumentation.rs`:

1. added `InstrKind::RetAnchorPush`
2. on `Return`, instrumented direct-ref carriers now export:
   - `RetAnchorPush` for the outer slot family
   - `RetLeafPush` for the exact nested pointer/ref leaves
3. caller-side `RetAnchorTake` now receives the anchor that was already assumed by the design

Observed result:

- `bytes` `split_off` / `split_to` representatives (`id:000000`, `id:000010`, `id:000022`) replay
  cleanly on the rebuilt instrumented harness
- the same return-boundary fix also closes the known TB-lite gap in
  `examples/miri_tests/sb_exact/src/bin/return_invalid_shr_tuple.rs`, so
  `expected.return_invalid_shr_tuple.tb_lite.rz` was updated from `ok` to the now-observed
  violation signature

## Next steps after this patch

1. Land `RetAnchorPush` and re-run:
   - bytes regressions
   - smallvec regressions
   - full default example suite
2. Reclassify the remaining live `bytes` crashes.
3. Then move to the `Bytes::slice` helper-local family bug, which is still the largest class.
