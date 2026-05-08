# Pointer-Shadow Memory for Provenance

## TODO

- [x] Add runtime pointer-shadow sidecar storage.

- [x] Add shadow store/load hooks for typed thin pointer slots.

- [x] Add shadow cleanup on allocation death and epoch removal.

- [x] Propagate shadow metadata through bytewise copy/move patterns.

- [x] Kill pointer shadow on overlapping non-pointer writes.

- [x] Validate the core paper examples:
  `lineage_static_smuggle`, `lineage_memcpy_roundtrip`, and `lineage_field_proj`.

- [x] Archive the completed core design under `tasks/done`.

- [ ] Future extension: wide pointers / DST metadata.

- [ ] Future extension: broader helper/wrapper reload shapes that do not currently materialize as
  direct shadowable loads.

- [ ] Future extension: integer round-trips, if a sound narrow design is established.

## Context

`rusteze` currently preserves most provenance through:
- compiler-created tag locals
- local MIR propagation
- runtime lineage repair keyed by address and allocation epoch

That works while pointer values stay in MIR locals. It breaks when a pointer value is:
- stored in a struct field
- copied through memory
- reloaded from a field, tuple, static, or bytewise copy

The current architecture therefore loses provenance in cases where Miri does not, because
Miri treats provenance as part of the pointer value and preserves it through memory.

Representative examples already in this repository:
- `paper_examples/lineage_field_proj`
- `paper_examples/lineage_memcpy_roundtrip`
- `paper_examples/lineage_static_smuggle`

Current status on `main`:
- `paper_examples/lineage_static_smuggle`
  - fixed by phase 1
  - now reports `TREE_BORROWS_VIOLATION|WRITE|RawMut|1`
- `paper_examples/lineage_memcpy_roundtrip`
  - fixed by phase 2
  - now reports `TREE_BORROWS_VIOLATION|WRITE|RawMut|1`
- `paper_examples/lineage_field_proj`
  - still a `--release` false negative
  - the remaining gap is field/projection-heavy reload paths whose lineage is still not materialized early enough for later ref/raw creation

## Goal

Make provenance a property of pointer values stored in memory, not only of MIR locals.

Concretely, the fix should allow `rusteze` to:
- preserve pointer lineage through memory-resident fields and aggregates
- preserve provenance through bytewise copies of pointer-carrying objects
- restore the correct tag when a pointer is reloaded from memory
- reduce dependence on root-like fallback tags for field projections and pointer smuggling

This does **not** by itself solve:
- opaque/uninstrumented return lineage loss
- generic `alias_exempt` limitations
- missing SSA ancestry that never touched memory

But it is the most important architectural fix because it closes the largest gap between
local-based metadata and value-based provenance.

## Why Current Repair Is Not Enough

Address-based lineage repair can reconnect a pointer only when:
- a relevant parent was already materialized
- the same address is visible in the same allocation epoch

It cannot recover provenance that was never stored anywhere except in the original local path.
When a pointer is copied through memory and later reloaded, the current runtime sees only:
- pointer bits
- current address

That is not enough to reconstruct the intended parent-child tree reliably.

## Design

### Overview

Add a shadow provenance store for pointer-typed values in program memory.

The basic rule is:
- when a pointer value is stored into memory, store its provenance in shadow metadata
- when a pointer value is loaded from memory, restore its provenance from that shadow metadata

Program memory still stores the real pointer bits.
Shadow memory stores the provenance attached to those bits.

### Where Metadata Lives

Use a per-allocation sidecar keyed by:
- allocation base
- allocation epoch
- offset within the allocation

Conceptually:

```text
(base, epoch) -> ptr_shadow[offset] = PtrShadowEntry
```

This should be implemented as a runtime side structure, not embedded directly in `AllocMeta`.

Reasons:
- cleanup is natural when an allocation dies
- address reuse is already handled by epochs
- it matches the existing allocation-centric runtime design

### Shadow Entry Payload

Phase 1 should support thin pointers only.

Minimum useful payload:

```text
PtrShadowEntry {
  width: usize,        // usually pointer size
  tag: u64,            // primary provenance tag
  ref_ancestor: u64,   // secondary lineage channel, if present
}
```

Optional future fields:
- flags for wide-pointer / DST handling
- pointee kind classification
- bounds metadata for wide pointers

### Required Runtime Operations

#### 1. Pointer store

When instrumented code stores a pointer-typed value to memory:

```text
shadow_store_ptr(addr, size, tag, ref_ancestor)
```

This records provenance for the pointer-sized slot beginning at `addr`.

#### 2. Pointer load

When instrumented code loads a pointer-typed value from memory:

```text
shadow_load_ptr(addr, size) -> (tag, ref_ancestor)
```

The result is used to initialize the synthetic tag locals associated with the destination
pointer local.

If no shadow entry exists:
- return `(0, 0)`

#### 3. Non-pointer write overlap

Any non-pointer write that overlaps a tracked pointer slot must conservatively kill shadow:

```text
shadow_kill_range(addr, size)
```

This is necessary for soundness under integer overwrites, partial updates, and memset-style writes.

#### 4. Memory copy / move

Bytewise copies of pointer-carrying objects must preserve shadow provenance:

```text
shadow_copy_range(dst, src, size)
```

This is required for:
- `memcpy`
- `memmove`
- custom bytewise copies lowered from MIR or wrapper calls

#### 5. Allocation death

When an allocation dies:
- drop all pointer-shadow entries for `(base, epoch)`

This should align with the current allocation-epoch cleanup path.

## Compiler-Side Instrumentation Changes

Main file:
- `instrument-mir/src/instrumentation.rs`

Add instrumentation support for:

### Pointer-typed stores

When the destination memory location is known to hold a pointer-typed value:
- emit normal memory write instrumentation
- additionally emit shadow provenance store

Examples:
- `field.ptr = p`
- `slot = p`
- storing pointer-valued tuple / aggregate fields

### Pointer-typed loads

When loading a pointer-typed value from memory into a local:
- emit normal load instrumentation
- additionally restore shadow provenance into the synthetic tag locals for the destination

Examples:
- `let p = field.ptr`
- `let p = tuple.0`
- loading from a static or stack slot

### Wrapper-based memory operations

Integrate shadow operations with:
- memcpy-like wrappers
- memmove-like wrappers
- memset / write_bytes wrappers

The existing wrapper table should become responsible for both:
- memory-access checks
- pointer-shadow propagation / invalidation

## Runtime Integration

Primary implementation points:
- `runtime/src/lib.rs`
- new module:
  - `runtime/src/ptr_shadow.rs`

The runtime should:
- resolve `addr` to `(base, epoch, offset)`
- query/update the sidecar shadow store
- invalidate or copy shadow ranges when memory operations occur
- clear the sidecar on allocation death

This feature should be orthogonal to the alias model:
- pointer shadow restores provenance
- `tb_lite` then uses that restored provenance to build/check the tree

## Implemented Phases

### Phase 1: typed pointer slots

Committed in:
- `db059f8` `runtime: add phase-1 pointer shadow provenance`

Delivered:
- shadow store/load for thin raw pointer slots
- shadow kill on overlapping writes
- cleanup on allocation death and epoch removal
- enough coverage to fix the static-smuggling case

### Phase 2: bytewise copy propagation

Committed in:
- `61e3ad7` `runtime: propagate pointer shadow through bytewise copies`

Delivered:
- `shadow_copy_range` runtime support
- MIR emission for memcpy-like wrappers and one-byte copy patterns
- partial-slot assembly so bytewise pointer reconstruction reconstitutes full shadow entries
- enough coverage to fix bytewise pointer copies such as `lineage_memcpy_roundtrip`

## Representative Cases

### 1. Field projection reload

`paper_examples/lineage_field_proj`

Current status:
- fixed

Delivered behavior:
- the pointer field store/load restores slot provenance
- the optimized-away source-level `&mut` binding is recovered by the compiler pass
- the later conflicting write is now reported as `TREE_BORROWS_VIOLATION|WRITE|RawMut|1`

### 2. Bytewise memory copy of pointer-carrying object

`paper_examples/lineage_memcpy_roundtrip`

Current status:
- fixed by phase 2

Delivered behavior:
- shadow copy mirrors the bytewise copy
- `dst.ptr` reload restores the same provenance as `src.ptr`
- the later conflicting write is now reported as `TREE_BORROWS_VIOLATION|WRITE|RawMut|1`

### 3. Static memory smuggling

`paper_examples/lineage_static_smuggle`

Current status:
- fixed by phase 1

Delivered behavior:
- the static slot carries provenance shadow
- the reloaded pointer remains connected to the original borrow tree
- the later conflicting write is now reported as `TREE_BORROWS_VIOLATION|WRITE|RawMut|1`

## Remaining Scope

The current implementation is still intentionally narrow:

- thin pointers only
- exact typed loads/stores of pointer-sized values
- bytewise copy propagation for memcpy/memmove-style transport
- overlapping non-pointer writes conservatively kill shadow

Still out of scope:
- wide pointers / DST metadata
- partial pointer-byte provenance
- integer round-trips
- helper/wrapper reload shapes that do not currently materialize as direct shadowable loads
- SSA-only ancestry that never touched memory

Those can come later.

## Soundness Rules

The feature must follow the repository soundness policy:
- missing metadata is acceptable
- incorrect metadata is not

Therefore:
- if shadow provenance cannot be restored confidently, return tag `0`
- if a non-pointer write overlaps a tracked slot, kill the shadow entry
- if a copy shape is not modeled, do not speculate

This may still miss bugs, but it should not fabricate lineage that is not justified.

## Implementation Order

1. Add runtime sidecar structure for pointer shadow
2. Add shadow store/load hooks for typed pointer stores and loads
3. Add shadow range copy / kill support for memcpy, memmove, memset, write_bytes
4. Hook cleanup into allocation death and epoch transitions
5. Validate on the paper examples listed above
6. Extend to wider aggregate and wrapper coverage only after the core path is stable

## Success Criteria

At minimum:
- existing default/interprocedural example suites must stay green
- the feature should remain sound by default or be gated behind an opt-in flag until proven safe
