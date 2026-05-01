# ShadowLoad and Carrier Leaf Shadow Transport

This note explains what `ShadowLoad` is in rusteze, and why `bytes` still exposes a false-positive
class around by-value carrier returns and copies.

## What `ShadowLoad` is

`ShadowLoad` is the instrumentation operation used when MIR loads a pointer value out of memory
into a local.

The pointer bits themselves are loaded by the original MIR assignment. `ShadowLoad` restores the
*metadata* for that pointer from rusteze's pointer-shadow side table:

- the pointer tag
- the `ref_ancestor`

Relevant code:

- `InstrKind::ShadowLoad`: [instrument-mir/src/instrumentation.rs](/home/ubuntu/rust-mir-instrumentor/instrument-mir/src/instrumentation.rs:1668)
- lowering of `ShadowLoad`: [instrument-mir/src/instrumentation.rs](/home/ubuntu/rust-mir-instrumentor/instrument-mir/src/instrumentation.rs:12404)
- runtime helpers:
  - [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs:2434)
  - [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs:2450)

Conceptually, `ShadowLoad` means:

```text
dst_ptr_bits = *slot
dst_tag = shadow_load_tag(slot_addr)
dst_ref_ancestor = shadow_load_ref_ancestor(slot_addr)
```

So `ShadowLoad` does not load the pointer value. It loads the shadow metadata that must travel
with that pointer value.

## Simple example

Rust source:

```rust
struct Wrap {
    p: *mut u8,
}

fn load(w: Wrap) -> *mut u8 {
    let q = w.p;
    q
}
```

MIR-ish shape:

```text
_2 = copy (_1.0: *mut u8)
```

The original MIR copies the bits of `_1.0` into `_2`.

rusteze then adds:

```text
ShadowLoad { dst_local: _2, place: (_1.0: *mut u8) }
```

Without `ShadowLoad`, `_2` would contain the right address bits but the hidden tag local for `_2`
would still be `0`. The first later use of `_2` would then degrade into `UNKNOWN_TAG`.

## Why pointer shadow exists

The pointer value alone is not enough for rusteze. The runtime also needs provenance / lineage
metadata:

- where this pointer family came from
- whether it is a ref or raw-derived pointer
- how alias checks should interpret later reads/writes

That metadata is kept out-of-band in pointer shadow, keyed by the memory slot that stores the
pointer.

So whenever code writes a pointer into memory, rusteze must do a `ShadowStore`.
Whenever code later reads that pointer back from memory, rusteze must do a `ShadowLoad`.

## The `bytes` problem

The subtle bug is not ordinary `ShadowLoad`. The bug is what happens when the source object is a
non-pointer carrier such as `BytesMut` or `Bytes`.

Those types are not themselves pointer-typed locals, but they contain internal pointer fields:

- `BytesMut`: [third_party/bytes/src/bytes_mut.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes_mut.rs:60)
- `Bytes`: [third_party/bytes/src/bytes.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes.rs:101)

For `BytesMut`, the important fields are:

```rust
pub struct BytesMut {
    ptr: NonNull<u8>,   // real data pointer
    len: usize,
    cap: usize,
    data: *mut Shared,  // real shared ptr in ARC mode, encoded metadata in VEC mode
}
```

The `data` field is important because in vec mode `bytes` stores compact metadata in that
pointer-typed slot by manufacturing a small invalid pointer value:

- [third_party/bytes/src/bytes_mut.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes_mut.rs:780)
- [third_party/bytes/src/bytes_mut.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes_mut.rs:1112)
- [third_party/bytes/src/bytes_mut.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes_mut.rs:1810)

So there are two different kinds of internal pointer-looking fields:

1. real provenance-bearing pointers, such as the actual data pointer
2. sentinel / metadata values such as `0x1`, which must be carried but not treated as real
   pointees

## Minimal carrier example

```rust
struct MiniBuf {
    ptr: *mut u8,   // real pointer
    len: usize,
    data: *mut u8, // sentinel / encoded metadata
}

fn make_buf(p: *mut u8) -> MiniBuf {
    MiniBuf {
        ptr: p,
        len: 0,
        data: 1usize as *mut u8,
    }
}

fn use_buf(b: MiniBuf) {
    let q = b.ptr;
    let s = b.data;

    unsafe {
        let _ = *q;
    }

    let _ = s;
}
```

Here:

- `p` is a real heap/data pointer
- `0x1` is a sentinel value, not a valid pointee

## Where rusteze loses precision

For by-value returns/copies of non-pointer carriers, rusteze currently restores only the
whole-object carrier anchor:

- caller-side return recovery for carriers:
  [instrument-mir/src/instrumentation.rs](/home/ubuntu/rust-mir-instrumentor/instrument-mir/src/instrumentation.rs:11108)

That restores the borrow family for the outer stack slot, conceptually:

```text
dst : MiniBuf belongs to family A
```

But it does **not** rebuild the exact leaf shadows for:

- `dst.ptr`
- `dst.data`

Later, when MIR projects one of those fields and loads it into a local, `ShadowLoad` reads the
field slot shadow.

If the field shadow was not reconstructed, `ShadowLoad` sees:

```text
load_tag(slot) -> 0
```

That gives the first false positive class:

```text
UNKNOWN_TAG
```

## Why the naive fix is wrong

The tempting repair is:

> if projected field shadow is missing, use the outer carrier anchor as the field parent

That is only valid for some projected reference-field cases. It is not valid for raw internal
fields.

Why not:

- the outer carrier anchor describes the stack slot `dst: MiniBuf`
- `dst.ptr` should keep the provenance of the real pointer `p`
- `dst.data = 0x1` is not a real pointee at all

If we rebuild `dst.ptr` or `dst.data` from the carrier anchor, we lie:

- the heap pointer gets a stack-slot family
- the sentinel gets treated as if it were a real borrowed location

That is exactly how the exploratory `bytes` fix failed:

1. it removed `UNKNOWN_TAG`
2. but then produced bogus `MISALIGNED_ACCESS` on `0x1`
3. and bogus `RAW_DERIVE_OOB` where a heap/internal pointer was derived under a stack-origin
   parent

## Before and after the patch

### Source example

```rust
#[derive(Clone, Copy)]
struct Carrier {
    ptr: *const u8,
    data: *mut u8,
}

fn ret(p: *const u8, d: *mut u8) -> Carrier {
    Carrier { ptr: p, data: d }
}

fn use_ret(p: *const u8, d: *mut u8) -> *const u8 {
    let c = ret(p, d);
    c.ptr
}
```

### MIR shape before patch

Caller side, conceptually:

```text
_1 = ret(_p, _d)
RetAnchorTake(callee_id, _1)
RetLeafTake(callee_id, leaf=0)   // positional
RetLeafTake(callee_id, leaf=1)   // positional
_2 = copy (_1.0: *const u8)
ShadowLoad { place: (_1.0), dst_local: _2 }
```

Callee side:

```text
_0 = Carrier { ptr: _p, data: _d }
RetLeafPush(callee_id, leaf=0)   // positional
RetLeafPush(callee_id, leaf=1)   // positional
return
```

The bug is that the transport key was just `leaf=0/1`. If the source and destination walks do not
line up exactly, the hidden shadow can cross fields:

```text
dst.ptr  <- src.data shadow
dst.data <- src.ptr shadow
```

Then the next projected field load does:

```text
ShadowLoad(dst.ptr)
```

with the right pointer bits but the wrong lineage.

### MIR shape after patch

Caller side:

```text
_1 = ret(_p, _d)
RetAnchorTake(callee_id, _1)
RetLeafTake(callee_id, key=offset(ptr))
RetLeafTake(callee_id, key=offset(data))
_2 = copy (_1.0: *const u8)
ShadowLoad { place: (_1.0), dst_local: _2 }
```

Callee side:

```text
_0 = Carrier { ptr: _p, data: _d }
RetLeafPush(callee_id, key=offset(ptr))
RetLeafPush(callee_id, key=offset(data))
return
```

Now the transport is field-keyed, not positional:

```text
dst.ptr  <- src.ptr shadow
dst.data <- src.data shadow
```

Lineage is still lost when the value crosses into a fresh caller slot, because shadow is keyed by
slot address. The patch restores it by recreating the exact leaf shadow for each field into the
new slot before later projected loads run.

## Correct patch

The patch is:

1. keep anchor repair only for the projected reference-field cases it was meant for
2. for by-value carrier returns/copies, transport the **exact leaf shadow** for each internal
   pointer field using a stable field key (prefer byte offset, fall back to a field-path key)

Conceptually:

### callee / source side

Export:

- the whole-slot carrier anchor
- leaf shadow for `RETURN_PLACE.ptr`
- leaf shadow for `RETURN_PLACE.data`

### caller / destination side

Import:

- the whole-slot carrier anchor for `dst`
- exact leaf shadow for `dst.ptr`
- exact leaf shadow for `dst.data`

So after return/copy:

```text
dst anchor      = family of outer carrier slot
dst.ptr shadow  = exact shadow of real internal pointer
dst.data shadow = exact shadow of sentinel/metadata field
```

Then later `ShadowLoad` on a projected field reads correct per-leaf metadata instead of either:

- `0`
- or a fabricated outer-anchor family

## Why this fixes the bug

This preserves the distinction between two different notions:

1. **outer carrier family**
   - which stack slot / borrow family owns the object
2. **inner pointer provenance**
   - what each internal pointer field actually means

The current false positives happen when rusteze conflates those two.

`ShadowLoad` itself is not the bug. `ShadowLoad` is where the bug becomes visible: it is the first
operation that asks, "what is the exact metadata for this field slot?" If the field shadow was not
transported precisely, `ShadowLoad` exposes that loss immediately.
