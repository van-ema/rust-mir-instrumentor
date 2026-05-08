# Bytes Interior Root Repair

This note documents the false-positive class that was blocking reliable `bytes` fuzzing.

## Problem shape

`BytesMut` is a non-pointer carrier object that contains pointer-bearing state. A call like:

```rust
fn poke(buf: &mut bytes::BytesMut) {
    let p = buf.as_mut_ptr();
    unsafe { *p = 0x41; }
}
```

can produce a protected whole-object family for `buf`, then later a separate raw/unique family for
the interior byte write.

Conceptually, the bad trace looked like this:

```text
133 = protected &mut BytesMut
136 = child family for the current call
149 = fresh root for buf.as_mut_ptr() interior byte
150 = write through 149
```

Tree Borrows then reports a protector conflict:

```text
WRITE via tag=150
protected_tag=133
reason=TB_LITE_PROTECTOR_CONFLICT
```

That is a false positive. The write is not foreign; rusteze lost ancestry and turned an interior
write into a detached root.

## Patch

The fix has two parts.

1. `instrument-mir/src/instrumentation.rs`

   For projected borrows/raw creations from non-pointer carrier locals that contain direct pointer
   fields, use the carrier anchor as the parent-family operand.

   That keeps projected source expressions inside the carrier family instead of defaulting to a
   projection-local root.

2. `runtime/src/lib.rs`

   If optimized MIR still emits a root (`parent=0`), try alloc-root repair in two stages:

   - exact same-address recovery from `lineage_cache` / `exact_parent_index`
   - otherwise, choose the smallest live enclosing non-root family in the same `alloc_epoch`

   The enclosing-range fallback is what reattaches interior byte/field roots under the live
   `BytesMut` family.

## Before / after

Before:

```text
&mut buf            -> tag 133
interior raw write  -> tag 149 (fresh root)
write child         -> tag 150
result              -> protector conflict against 133
```

After:

```text
&mut buf            -> tag 133
interior raw write  -> repaired/anchored under 133/136
write child         -> descendant of the live buf family
result              -> no foreign-write protector conflict
```

## Why this is the right fix

We are not weakening Tree Borrows. We are restoring the missing parent family so the write is
checked in the correct lineage. The violation disappeared because the metadata became correct, not
because the rule was relaxed.
