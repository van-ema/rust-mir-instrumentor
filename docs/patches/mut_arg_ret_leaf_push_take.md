# `MutArgRetLeafPush` / `MutArgRetLeafTake`

`MutArgRetPush` / `MutArgRetTake` repair the **outer family tag** for a `&mut T`
carrier pointee after a call returns.

That is necessary, but not sufficient, when `T` is a non-pointer carrier with
internal pointer leaves, such as `BytesMut` or `Bytes`.

## Problem

Example:

```rust
fn callee(buf: &mut bytes::BytesMut) {
    buf.reserve(8);
}

fn caller(mut buf: bytes::BytesMut) {
    callee(&mut buf);
    let p = buf.as_mut_ptr();
    unsafe { *p = 1; }
}
```

`BytesMut` does not just carry one pointer. It carries internal pointer leaves
such as:

- `buf.ptr`
- `buf.data`

The call can legally rewrite those leaves.

Before the patch, the return edge only restored the outer carrier family:

```text
caller:
    _arg = &mut buf;
    call callee(_arg)
    MutArgRetTake(callee_id, arg_index, buf_slot)
```

That tells rusteze:

- the caller slot `buf` is still the live carrier family

But later projected loads use the **leaf slots**, not the outer slot:

```text
_p = copy (buf.ptr)
```

At that point `ShadowLoad` asks for the shadow of the `buf.ptr` slot itself.
If the callee changed that field and we only restored the outer family, the leaf
slot can still be missing or stale.

## Patch

The patch mirrors `RetLeafPush` / `RetLeafTake`, but for `&mut T` writeback.

### Callee side

Immediately before `Return`, export:

1. the outer family tag for `*arg`
2. the exact shadow of each internal pointer leaf of `*arg`

Pseudo-MIR:

```text
Return:
    MutArgRetPush(callee_id, arg_index, *arg)
    MutArgRetLeafPush(callee_id, arg_index, (*arg).ptr,  key=offset(ptr))
    MutArgRetLeafPush(callee_id, arg_index, (*arg).data, key=offset(data))
    return
```

### Caller side

Right after the call returns, import:

1. the outer family for the caller pointee slot
2. each exported leaf shadow into the caller leaf slot

Pseudo-MIR:

```text
call callee(&mut buf) -> bb_ret

bb_ret:
    MutArgRetTake(callee_id, arg_index, buf)
    MutArgRetLeafTake(callee_id, arg_index, buf.ptr,  key=offset(ptr))
    MutArgRetLeafTake(callee_id, arg_index, buf.data, key=offset(data))
```

## Why lineage is lost

The shadow store is keyed by the **slot address** that currently holds the
pointer value.

So after a call boundary we need to restore two different things:

- the outer slot lineage for `buf`
- the exact leaf lineage for `buf.ptr`, `buf.data`, ...

If we restore only the outer slot, later field loads still see:

```text
ShadowLoad(buf.ptr) -> tag 0
```

and the next dereference/write becomes `UNKNOWN_TAG`.

## Runtime hooks

The MIR patch is implemented with two side-channel hooks:

- `__rz_push_mut_arg_ret_leaf_shadow`
- `__rz_take_mut_arg_ret_leaf_shadow`

They transport:

- `tag`
- `ref_ancestor`

for one leaf at a time, keyed by:

- thread id
- callee id
- argument index
- pointee base address
- leaf key (byte offset when layout is known, otherwise a stable field-path key)

That keeps the outer-family repair (`MutArgRetTake`) separate from the exact
leaf-slot repair (`MutArgRetLeafTake`).
