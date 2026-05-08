# `&mut T` carrier writeback

This note documents the call-boundary fix for non-pointer carrier locals mutated through `&mut T`.

## Problem

For plain pointer locals, rusteze can carry lineage in the pointer tag itself.

For carrier locals such as:

- `BytesMut`
- `Option<&T>`
- `struct Wrap<'a> { r: &'a T }`

the local is not pointer-typed, but later borrows from that local still need a parent family.

Rusteze tracks that family in a hidden anchor local next to the carrier local.

The bug class was:

1. caller passes `&mut carrier` to a callee
2. callee overwrites or mutates the carrier contents
3. caller later re-borrows from the carrier
4. rusteze still uses the pre-call family, or a transient callee child tag, as the parent

That produced `bytes` false positives in nested `BytesMut` call chains.

## Example

Rust:

```rust
struct Wrap<'a> {
    r: &'a u8,
}

fn update<'a>(w: &mut Wrap<'a>, x: &'a u8) {
    w.r = x;
}

fn caller(a: &u8, b: &u8) {
    let mut w = Wrap { r: a };
    update(&mut w, b);
    let _ = *w.r;
}
```

Before the fix, `caller` could keep using the old family for `w` after `update` returned.

## Mechanism

The writeback path is now explicit.

### 1. Caller -> callee

Before the call, the caller pushes the current family for the pointee slot of `&mut T`.

Instrumentation:

- `InstrKind::CallArgPush`

Runtime:

- `__rz_push_call_arg_tag`

### 2. Callee entry

The callee imports that family and retags its local view.

Instrumentation:

- pointer args: `InstrKind::ArgRetag`
- carrier args: `InstrKind::ArgAnchorTake`

Runtime:

- `__rz_take_call_arg_tag`
- `__rz_take_call_arg_tag_anchor`

### 3. Callee return

If the callee took `&mut T` where `T` is a non-pointer carrier with tracked pointer fields, it now exports the updated family for that pointee slot.

Instrumentation:

- `InstrKind::MutArgRetPush`

Runtime:

- `__rz_push_mut_arg_ret_tag`

Important detail: this channel must export a stable family tag, not an ephemeral inner child created during the callee body. For `tb_lite`, the runtime now prefers the nearest surviving exact-slot `Unique` family over either:

- a temporary inner child created by nested helper calls
- an older root ref family for the same slot

### 4. Caller return edge

After the call, the caller consumes the exported family and refreshes:

- the carrier anchor for the pointee local
- the tag / ref-ancestor of the caller pointer local when that `&mut` local itself stays live after the call

Instrumentation:

- `InstrKind::MutArgRetTake`

Runtime:

- `__rz_take_mut_arg_ret_tag`

## Pseudo-MIR shape

Caller side:

```text
_w = Wrap { r: a };
_arg = &mut _w;
Call(update(move _arg, b)) -> bb1;

bb1:
  _ret_family = __rz_take_mut_arg_ret_tag(callee_id, 0, addr_of(_w));
  _anchor_w = _ret_family != 0 ? _ret_family : _anchor_w;
  _tag_arg  = _ret_family != 0 ? _ret_family : _tag_arg;
```

Callee side:

```text
bb_ret:
  __rz_push_mut_arg_ret_tag(callee_id, 0, addr_of((*_arg)), current_family);
  Return
```

## Relevant code

- `instrument-mir/src/instrumentation.rs`
  - `InstrKind::MutArgRetPush`
  - `InstrKind::MutArgRetTake`
- `runtime/src/lib.rs`
  - `__rz_push_mut_arg_ret_tag`
  - `__rz_take_mut_arg_ret_tag`
- `runtime/src/alias_model/tree_borrows_lite.rs`
  - TB-specific family canonicalization used by the writeback channel

## Current status

This fixes the original `bytes` stale-family crashes on:

- projectionless `&mut` carrier reborrows
- caller-side carrier-anchor staleness after nested calls
- caller-side stale `&mut` local tags after nested calls

There is still at least one remaining `bytes` TB false-positive class in nested call/protector handling. This note only documents the carrier writeback mechanism.
