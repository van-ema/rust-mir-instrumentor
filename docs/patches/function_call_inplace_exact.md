# Function-Call In-Place Exact Case

This note documents the `miri_function_calls_exact` in-place argument case:

- `arg_inplace_mutate`
- `arg_inplace_observe_during`

These tests are interesting because Miri evaluates the custom MIR as an abstract call-boundary
semantics test, while rusteze observes the lowered executable MIR plus the concrete machine
addresses used at runtime.

## The source test Miri sees

The upstream tests are written with `custom_mir`:

```rust
pub struct S(i32);

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        let _unit: ();
        {
            let non_copy = S(42);
            let ptr = std::ptr::addr_of_mut!(non_copy);
            Call(_unit = callee(Move(*ptr), ptr), ReturnTo(after_call), UnwindContinue())
        }
        after_call = {
            Return()
        }
    }
}
```

and:

```rust
pub struct S(i32);

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        let _unit: ();
        {
            let non_copy = S(42);
            let ptr = std::ptr::addr_of_mut!(non_copy);
            Call(_unit = change_arg(Move(*ptr), ptr), ReturnTo(after_call), UnwindContinue())
        }
        after_call = {
            Return()
        }
    }
}
```

The key operation is:

```text
Call(_unit = callee(Move(*ptr), ptr), ...)
Call(_unit = change_arg(Move(*ptr), ptr), ...)
```

Miri treats this as an exact custom-MIR call. In that model, the by-value argument `Move(*ptr)`
can behave like an in-place transfer of the pointee storage into the callee argument local.

That is why the tests expect the callee argument and `*ptr` to be treated as aliasing places for
Tree Borrows purposes.

## Why Miri rejects them

For `arg_inplace_mutate`:

```rust
fn callee(x: S, ptr: *mut S) {
    unsafe { ptr.write(S(0)) };
    assert_eq!(x.0, 42);
}
```

If `x` is the in-place carrier for the moved `*ptr`, then writing through `ptr` writes the same
storage that now belongs to `x`. Tree Borrows rejects that write.

For `arg_inplace_observe_during`:

```rust
fn change_arg(mut x: S, ptr: *mut S) {
    x.0 = 0;
    unsafe { ptr.read() };
}
```

If `x` was passed in place from `*ptr`, then mutating `x` and then reading through `ptr` is an
aliasing violation. Tree Borrows rejects that read.

## What rusteze sees after lowering and instrumentation

rusteze does not execute the abstract custom MIR directly. It instruments the lowered MIR that
rustc emits for normal execution.

For `arg_inplace_mutate`, the caller side becomes:

```text
bb5: {
    _1 = callee(move (*_3), copy _3) -> [return: bb1, unwind continue];
}

bb7: {
    _23 = copy _3 as usize (PointerExposeProvenance);
    _24 = runtime::__rz_push_call_arg_tag(..., const 1_u64, copy _23, copy _4) -> ...;
}

bb8: {
    _21 = copy _3 as usize (PointerExposeProvenance);
    _26 = &raw const (*_3);
    _25 = copy _26 as usize (PointerExposeProvenance);
    _27 = runtime::__rz_push_call_arg_tag(..., const 0_u64, copy _25, copy _4) -> ...;
}

bb9: {
    _3 = &raw mut _2;
    _28 = copy _3 as usize (PointerExposeProvenance);
    _4 = runtime::__record_raw_ptr_creation(...) -> ...;
}
```

and the callee starts as:

```text
fn callee(_1: S, _2: *mut S) -> () {
    ...
    _45 = &raw const _1;
    _44 = copy _45 as usize (PointerExposeProvenance);
    _46 = runtime::__rz_take_call_arg_tag_anchor(..., const 0_u64, copy _44) -> ...;
}
```

For `arg_inplace_observe_during`, the same pattern appears:

```text
bb5: {
    _1 = change_arg(move (*_3), copy _3) -> [return: bb1, unwind continue];
}

bb7: {
    _23 = copy _3 as usize (PointerExposeProvenance);
    _24 = runtime::__rz_push_call_arg_tag(..., const 1_u64, copy _23, copy _4) -> ...;
}

bb8: {
    _21 = copy _3 as usize (PointerExposeProvenance);
    _26 = &raw const (*_3);
    _25 = copy _26 as usize (PointerExposeProvenance);
    _27 = runtime::__rz_push_call_arg_tag(..., const 0_u64, copy _25, copy _4) -> ...;
}
```

and:

```text
fn change_arg(_1: S, _2: *mut S) -> () {
    ...
    _45 = &raw const _1;
    _44 = copy _45 as usize (PointerExposeProvenance);
    _46 = runtime::__rz_take_call_arg_tag_anchor(..., const 0_u64, copy _44) -> ...;
}
```

So rusteze does preserve tag flow across the call boundary:

- it pushes metadata for the raw-pointer argument
- it pushes metadata for the dereferenced by-value argument
- it re-attaches that metadata to the callee argument local `_1`

But that still operates on the actual callee local `_1`, not on an abstract "same storage as
caller `*ptr`" location.

## The concrete runtime evidence

For `arg_inplace_mutate`, the Tree-Borrows trace shows the two accesses land on different
addresses:

```text
[tb-trace] access=Write orig_tag=3 access_tag=3 addr=0x7ffcd1af4704 size=4 kind=RawMut lineage=[3, 1]
[tb-trace] access=Read  orig_tag=5 access_tag=5 addr=0x7ffcd1af4634 size=4 kind=RefShared lineage=[5, 4, 2]
```

The write through `ptr` hits `0x7ffcd1af4704`.

The read of the callee local `x` hits `0x7ffcd1af4634`.

Those are different locations. In the lowered native execution that rusteze instruments, the
callee local and the caller pointee are not occupying the same concrete slot.

That is the key difference from the abstract custom-MIR interpretation Miri is testing.

## Why this creates the mismatch

Miri is checking a call-boundary aliasing rule over the exact custom MIR:

- "`Move(*ptr)` may reuse the same logical place as the callee by-value argument"

rusteze is checking the lowered executable program:

- "`_1: S` is a concrete callee local with its own address"
- "`_2: *mut S` points at the caller storage"

Once lowering separates those into different concrete slots, plain address-based shadow state plus
ordinary tag transport is not enough to reproduce the exact Miri result.

In short:

- Miri sees a logical in-place move relation
- rusteze sees two different runtime addresses

## How rusteze matches Miri

rusteze now uses an additional virtual call-boundary model for by-value arguments moved out of
dereferenced places.

The caller already pushes two call-side-channel entries for this shape:

- argument 0 for the by-value `Move(*ptr)` source
- argument 1 for the raw pointer `ptr`

When the callee consumes the raw-pointer argument tag and the side channel still contains another
argument with the same tag and address for the same call, the runtime reports that to the active
alias model through `on_call_arg_inplace_alias`.

Tree-Borrows-lite records that parent tag in the current call frame. When the callee then creates
the raw child tag for `ptr`, TB-lite marks that child as an in-place protected call argument.
Any read or write through that protected raw child while the call frame is active reports:

```text
reason=TB_LITE_INPLACE_CALL_ARG
```

This is deliberately narrower than forcing the callee local `_1` and caller `*ptr` concrete
addresses to alias globally. The virtual relation exists only for the dynamic call frame and only
when the call side-channel proves duplicate same-tag/same-address arguments.

## Practical conclusion

This case is not a generic Tree-Borrows bug in the current runtime state machine.

It is a custom-MIR call-boundary case:

- Miri compares against exact custom MIR semantics
- rusteze instruments lowered executable MIR plus concrete addresses
- rusteze adds a narrow virtual in-place call relation when call-side-channel metadata identifies
  the exact duplicate-argument pattern

So the fix is not "more aggressive retagging". The fix is a dedicated virtual in-place
call-argument model that survives MIR lowering.
