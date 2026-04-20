# Runtime Data Structures

This note summarizes the main runtime metadata structures used by `rusteze`, with emphasis on
pointer shadow state and how it is stored.

It is not a complete runtime design document. The goal is to give a reviewer enough structure to
understand what the important records are and why they exist.

## 1. `TagMeta`

Defined in the runtime tag store.

Purpose:

- represents the metadata associated with one logical pointer tag
- used by memory-safety checks and alias-model checks

Typical fields include:

- pointee address
- pointer kind (`RefShared`, `RefMut`, `RawConst`, `RawMut`, ...)
- parent tag
- allocation epoch
- bounds length
- origin allocation snapshot
- alias-exempt flags / hints

What it answers:

- what memory region does this tag refer to?
- what is its parent lineage?
- what kind of pointer/reference is it?
- what allocation / epoch was it derived from?

This is the main per-tag record used by access checks such as:

- `__rz_ptr_read`
- `__rz_ptr_write`
- alias-model `check_access(...)`

## 2. `PtrShadowEntry`

Defined in:

- [ptr_shadow.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/ptr_shadow.rs)

Shape:

```rust
struct PtrShadowEntry {
    tag: u64,
    ref_ancestor: u64,
    alloc_base: usize,
    alloc_epoch: u64,
}
```

Purpose:

- represents the pointer metadata stored for one pointer-sized memory slot

Meaning of fields:

- `tag`
  - the current tag of the pointer value stored in that slot
- `ref_ancestor`
  - companion lineage anchor used for later ref/raw derivations
- `alloc_base`
  - allocation base snapshot for validating absolute shadow entries
- `alloc_epoch`
  - allocation epoch snapshot for reuse detection

What it answers:

- if a pointer value is loaded from this memory slot, what tag should the loaded local get?
- what ref-ancestor should also be restored?
- is this shadow entry still valid for the current allocation epoch?

This is the core record used by:

- `__rz_shadow_store_ptr`
- `__rz_shadow_load_tag`
- `__rz_shadow_load_ref_ancestor`

## 3. `PartialPtrShadowEntry`

Also defined in:

- [ptr_shadow.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/ptr_shadow.rs)

Shape:

```rust
struct PartialPtrShadowEntry {
    tag: u64,
    ref_ancestor: u64,
    valid_mask: u128,
    poisoned: bool,
}
```

Purpose:

- tracks partially valid pointer bytes for cases where a pointer-sized slot is only partially
  initialized or partially overwritten

Meaning of fields:

- `tag`
  - pointer tag associated with the partial bytes
- `ref_ancestor`
  - companion lineage anchor
- `valid_mask`
  - which bytes of the pointer-sized slot are still valid
- `poisoned`
  - whether the partial state should be treated as poisoned / invalid

Why it exists:

- plain full-slot shadow entries are not enough when memory operations affect only part of a stored
  pointer
- partial tracking prevents stale or fabricated full-pointer metadata from surviving bytewise
  updates

## 4. How pointer shadow is stored

There are two main storage modes for full pointer shadow:

### A. allocation-relative shadow

Type:

```rust
HashMap<(usize, u64), BTreeMap<usize, PtrShadowEntry>>
```

Stored in:

- `ALLOC_PTR_SHADOW`

Key meaning:

- outer key `(base, epoch)` identifies a live allocation snapshot
- inner key `offset` identifies the pointer-sized slot within that allocation

Why this mode exists:

- it keeps slot metadata attached to allocation identity
- it naturally handles allocation reuse through epoch separation

When it is used:

- the slot address currently belongs to a live tracked allocation
- the full pointer-sized slot fits inside that allocation

Key property:

- the lookup key is not just the raw address
- it is `(allocation base, allocation epoch, slot offset)`
- so if the allocator later reuses the same raw address with a different epoch, old shadow state
  does not automatically alias the new allocation

### B. absolute-address shadow

Type:

```rust
BTreeMap<usize, PtrShadowEntry>
```

Stored in:

- `ABS_PTR_SHADOW`

Key meaning:

- absolute address of the pointer slot

Why this mode exists:

- fallback for addresses not currently tied to a live tracked allocation
- backup path for loads when allocation-relative lookup is unavailable

The runtime validates these entries with the saved `alloc_base` / `alloc_epoch` snapshot before
trusting them for a current load.

When it is used:

- the slot address does not currently resolve to a suitable live tracked allocation
- or the runtime wants a raw-address fallback entry in addition to the allocation-relative one

Key property:

- the key is just the absolute slot address
- so by itself it is weaker than allocation-relative storage
- that is why each entry also stores `alloc_base` / `alloc_epoch`, and loads re-check that snapshot
  before accepting the entry as still valid

## 4.1 Practical difference: same raw address, different allocation

Suppose a pointer slot lived at raw address `0x1000` in allocation epoch `7`, and later the
allocator reuses the same raw address for a different allocation at epoch `8`.

### `ALLOC_PTR_SHADOW`

The old entry is keyed under something like:

```text
(base=0x1000, epoch=7, offset=0)
```

The new allocation will look under:

```text
(base=0x1000, epoch=8, offset=0)
```

So the old and new shadow states are naturally separated.

### `ABS_PTR_SHADOW`

The key is only:

```text
addr = 0x1000
```

So an old absolute entry could collide with later reuse of the same raw address. That is why
absolute entries carry `alloc_base` / `alloc_epoch` snapshots and are only trusted if that
snapshot still matches the current live allocation.

Short version:

- `ALLOC_PTR_SHADOW` is the strong, allocation-identity-aware store
- `ABS_PTR_SHADOW` is the raw-address fallback store with extra validation

## 5. Partial shadow storage

There are parallel maps for partial pointer shadow:

### allocation-relative partial shadow

```rust
HashMap<(usize, u64), BTreeMap<usize, PartialPtrShadowEntry>>
```

- `ALLOC_PTR_SHADOW_PARTIAL`

### absolute-address partial shadow

```rust
BTreeMap<usize, PartialPtrShadowEntry>
```

- `ABS_PTR_SHADOW_PARTIAL`

These are used when the runtime cannot treat a slot as a clean full pointer-sized store/load.

## 6. `SlotLoc`

Defined in:

- [ptr_shadow.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/ptr_shadow.rs)

Shape:

```rust
enum SlotLoc {
    Alloc { base, epoch, offset },
    Abs { addr },
}
```

Purpose:

- classifies a slot address into either:
  - allocation-relative location
  - absolute-address fallback location

Used by:

- `store_ptr`
- `load_entry`
- `kill_range`

This is the routing decision that determines which shadow map is consulted or updated.

## 7. How `__rz_shadow_store_ptr` uses these structures

When instrumentation calls:

```text
__rz_shadow_store_ptr(slot_addr, tag, ref_ancestor)
```

the runtime:

1. computes the slot location (`SlotLoc`)
2. clears overlapping existing shadow state for one pointer-sized slot
3. builds a `PtrShadowEntry`
4. stores it into:
   - allocation-relative shadow if the slot belongs to a live tracked allocation
   - absolute-address shadow otherwise

So `PtrShadowEntry` is the actual record written by `__rz_shadow_store_ptr`.

## 8. Why both tag store and pointer shadow exist

These serve different roles:

- `TagMeta`
  - metadata for a logical tag
  - answers “what does tag 417 mean?”
- `PtrShadowEntry`
  - metadata for a memory slot currently storing a pointer value
  - answers “what tag should I restore if I load a pointer from address X?”

Short version:

- tag store = per-tag metadata
- pointer shadow = per-memory-slot metadata

Both are needed:

- the tag store lets checks interpret tags
- pointer shadow lets loads/stores preserve tags through memory

## 9. Review takeaway

If you are reviewing runtime metadata flow, the most important split to keep in mind is:

1. `TagMeta`
   - meaning of a tag
2. `PtrShadowEntry`
   - which tag is stored in a slot
3. allocation-relative vs absolute shadow maps
   - where slot metadata is stored and how reuse is handled
