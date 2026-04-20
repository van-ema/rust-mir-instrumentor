# `ShadowStoreBoxPointee` and `box_pointee_slot_addr_stmts_for_local`

This note explains the special-case MIR instrumentation used when a pointer value is wrapped into a
`Box<T>` and stored in heap memory.

## Problem

Normal shadow stores handle assignments into ordinary memory places:

```rust
x = p;
s.field = p;
arr[i] = p;
```

In those cases, the instrumentation can compute the destination slot address directly from the
destination place and write the pointer's tag metadata there.

`Box::new(p)` is different. The pointer payload is written into newly allocated heap memory inside
library code, and after the call returns we only have the resulting `Box<T>` handle. If we do not
write shadow metadata into the box payload, later loads from the box can lose the original pointer
lineage.

## The special instruction

`InstrKind::ShadowStoreBoxPointee { box_local, src_local }` means:

- `src_local` is a pointer local whose tag metadata we want to preserve
- `box_local` is a MIR local of type `Box<T>`
- write the shadow metadata of `src_local` into the heap slot owned by `box_local`

This is a metadata-preservation step, not a new alias-model rule.

## Example in Rust source

```rust
fn wrap(p: *mut u8) -> Box<*mut u8> {
    Box::new(p)
}
```

Semantically:

- `p` already has a tag / ref-ancestor lineage
- `Box::new(p)` allocates heap storage for `*mut u8`
- the value `p` is copied into that heap slot
- later code might load it back:

```rust
let boxed = wrap(p);
let q = *boxed;
```

For `q` to recover the right lineage, the heap slot inside `boxed` must already contain the shadow
metadata for `p`.

## Simplified MIR shape

Source:

```rust
fn wrap(p: *mut u8) -> Box<*mut u8> {
    Box::new(p)
}
```

Simplified MIR:

```text
_0 = Box::<*mut u8>::new(copy _1);
return;
```

Where:

- `_1` is `p: *mut u8`
- `_0` is the return local `Box<*mut u8>`

The instrumentation recognizes:

- the destination local is `Box<T>`
- the call has one argument
- the first argument is a shadowable pointer local
- the first argument is a plain local place

At that point it inserts:

```text
ShadowStoreBoxPointee { box_local: _0, src_local: _1 }
```

## What `box_pointee_slot_addr_stmts_for_local` does

`box_pointee_slot_addr_stmts_for_local` computes the heap payload address of the box pointee.

Conceptually it does:

1. inspect the `Box<T>` local type
2. walk through the box representation to its internal `NonNull<T>`
3. cast that internal pointer to `*const u8`
4. expose provenance to get a `usize` address

This gives the runtime a concrete memory slot address for the box payload.

## Simplified lowered effect

Conceptually, the instrumented code behaves like:

```text
_0 = Box::<*mut u8>::new(copy _1);

_tmp_ptr = transmute(box_payload_ptr(_0));
_tmp_addr = expose_provenance(_tmp_ptr);
__rz_shadow_store_ptr(_tmp_addr, tag(_1), ref_ancestor(_1));
```

The runtime entry point is:

```text
__rz_shadow_store_ptr(slot_addr, tag, ref_ancestor)
```

which writes the pointer metadata into shadow memory for that heap slot.

## Why ordinary `ShadowStore` is not enough

Ordinary `ShadowStore` expects a normal destination memory place. For `Box::new(p)`, the actual
store into the pointee happens inside the box allocation path, so the pass cannot simply take the
address of a user-visible destination place in the caller.

Instead it:

- waits until the box value exists
- reconstructs the box payload address from the returned `Box<T>`
- writes the shadow metadata explicitly with `ShadowStoreBoxPointee`

## Summary

- `ShadowStoreBoxPointee` preserves pointer lineage through `Box<T>` payload storage
- `box_pointee_slot_addr_stmts_for_local` computes the heap slot address needed for that store
- this prevents tag loss for patterns like `Box::new(p)` followed by `let q = *boxed`
