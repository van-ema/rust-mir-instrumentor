# `CallArgValidate` and `RetValidate`

This note explains the boundary-validation hooks used for non-pointer carrier values that still
contain references.

## Problem

Some values crossing a call or return boundary are not themselves pointer-typed, but still carry a
reference inside an aggregate/container.

Examples:

```rust
Option<&i32>
(&i32, bool)
struct Wrap<'a> { r: &'a i32 }
```

If instrumentation only tracks plain pointer locals, these cases can silently skip boundary
validation even though an invalid reference is being passed or returned.

## The hooks

Two MIR instrumentation kinds handle this:

- `InstrKind::CallArgValidate { local }`
- `InstrKind::RetValidate { local }`

There is no `RetArgValidate` instruction in the current code. The return-side hook is
`RetValidate`.

## `CallArgValidate`

Meaning:

- a call argument local is not pointer-typed
- but its type contains a reference/raw pointer field
- recover the inner reference lineage from the carrier local
- validate that reference immediately before the call

### Rust example

```rust
fn sink(x: Option<&i32>) {}

fn demo() {
    let mut v = 0;
    let r = &v;
    let p = &mut v;
    *p = 1;
    sink(Some(r));
}
```

Here `Some(r)` is not a plain pointer local. Without special handling, the invalid shared ref can
cross the call boundary hidden inside `Option<&i32>`.

### Simplified MIR shape

```text
_1 = &v;
_2 = &mut v;
(*_2) = 1;
_3 = Option::<&i32>::Some(copy _1);
Call sink(move _3);
```

Instrumented idea:

```text
_3 = Option::<&i32>::Some(copy _1);
Call __rz_validate_call_arg_tag(tag_of_inner_ref(_3));
Call sink(move _3);
```

The runtime entry point is:

```text
__rz_validate_call_arg_tag(tag)
```

which performs a read-style alias-model check on the recovered reference tag before the call
proceeds.

## `RetValidate`

Meaning:

- the function return local is not pointer-typed
- but the return type contains a reference/raw pointer field
- recover the inner reference lineage from `RETURN_PLACE`
- validate that reference immediately before `Return`

### Rust example

```rust
fn produce<'a>(x: &'a i32) -> Option<&'a i32> {
    Some(x)
}
```

The returned value is a wrapper carrier, not a plain `&i32`. Pointer returns use
`RetPush` / `RetTake`; wrapper returns need a separate validation hook.

### Simplified MIR shape

```text
_0 = Option::<&i32>::Some(copy _1);
return;
```

Instrumented idea:

```text
_0 = Option::<&i32>::Some(copy _1);
Call __rz_validate_ret_tag(tag_of_inner_ref(_0));
return;
```

The runtime entry point is:

```text
__rz_validate_ret_tag(tag)
```

which performs the same boundary check on the inner reference tag before control returns to the
caller.

## Why push/take is not enough

Plain pointer arguments/returns already have dedicated call-boundary machinery:

- `CallArgPush` / `ArgRetag`
- `RetPush` / `RetTake`

That machinery assumes the ABI value being passed around is itself a pointer local. For
`Option<&T>`, tuples, or wrapper structs, that assumption is false, so the boundary would otherwise
be missed.

`CallArgValidate` and `RetValidate` fill exactly that gap.

## Summary

- `CallArgValidate` handles non-pointer argument carriers with inner refs
- `RetValidate` handles non-pointer return carriers with inner refs
- caller-side anchor import for those return carriers is handled separately by
  `RetAnchorTake`
- both exist to catch invalid references crossing boundaries inside wrappers like `Option<&T>` or
  tuples
