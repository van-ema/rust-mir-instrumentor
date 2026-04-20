# Shadow Operations

This note summarizes the MIR instrumentation kinds that keep pointer shadow metadata in sync with
ordinary program memory.

The general rule is:

- when the program stores, loads, copies, or overwrites pointer values in memory
- rusteze must also update the corresponding shadow metadata for those memory slots

## `ShadowStore`

Meaning:

- a pointer local is being written into an ordinary memory slot
- store that pointer's tag metadata into shadow memory for the destination slot

Example:

```rust
let p: *mut u8 = ...;
let mut x: *mut u8 = std::ptr::null_mut();
x = p;
```

Conceptually:

```text
slot_addr = addr_of(x);
__rz_shadow_store_ptr(slot_addr, tag(p), ref_ancestor(p));
x = p;
```

Without this, later loads from `x` can lose pointer lineage.

## `ShadowLoad`

Meaning:

- a pointer local is being loaded from memory
- restore the pointer tag metadata for the loaded local from shadow memory

Example:

```rust
let q = x;
```

If `x`'s slot previously had shadow metadata from `ShadowStore`, `ShadowLoad` restores it onto
`q`.

## `ShadowKill`

Meaning:

- a memory range is being overwritten in a way that destroys any stored pointer value
- clear shadow metadata for that range

This prevents stale pointer metadata from surviving after the program overwrites the real bytes.

## `ShadowCopySlot`

Meaning:

- copy shadow metadata from one memory slot to another

Used when MIR/byte-copy logic copies memory slots and we want the shadow state to follow the real
memory contents.

## `ShadowCopyRange`

Meaning:

- copy shadow metadata across a byte range between memory locations

This is the range-based version used for bulk byte copies.

## `ShadowStoreBoxPointee`

Meaning:

- a pointer value is being wrapped into a `Box<T>` where `T` is pointer-typed
- write the pointer metadata into the heap pointee slot owned by the box

This is the special-case heap version of `ShadowStore`.

Example:

```rust
fn wrap(p: *mut u8) -> Box<*mut u8> {
    Box::new(p)
}
```

Here the pointer payload is stored into newly allocated heap memory, so rusteze must also store
the shadow metadata into the box payload slot.

See:

- [shadow_store_box_pointee.md](./shadow_store_box_pointee.md)

## Summary

- `ShadowStore`: write pointer metadata into a normal memory slot
- `ShadowLoad`: restore pointer metadata from a memory slot into a local
- `ShadowKill`: clear stale pointer metadata after overwrite
- `ShadowCopySlot` / `ShadowCopyRange`: move shadow metadata with copied memory
- `ShadowStoreBoxPointee`: special-case `Box<T>` heap payload storage
