# `bytes::Bytes::slice` dead-stack false positive

## Symptom

On this branch, `afl_bytes_driver` could crash in `bytes::Bytes::slice` with:

- `USE_AFTER_DEAD`
- `reason=REF_CREATE_FROM_DEAD_ALLOC`

The failing path was a shared-ref creation for an inner `&usize` extracted from a
`Bound<&usize>` carrier while slicing a `Bytes`.

## Root cause

The bug was in compiler-side stack-slot tracking, not in the runtime checks.

Two issues combined here:

1. fixed-size stack locals were treated as "do not emit `StackAlloc`", because
   `SizeOperand::Const(_)` was incorrectly used as a skip condition instead of only treating
   synthetic `size=0` as unsupported;
2. once fixed-size emission was enabled, the final lowering step for `InstrKind::StackAlloc`
   still dropped the relevant fallback-entry hook for the `impl RangeBounds<usize>` argument
   slot in `bytes::Bytes::slice`, because it did this:

```rust
let ptr_ty = Ty::new_imm_ptr(tcx, local_ty);
if !self.is_addr_exposable_ptr_ty(tcx, body, ptr_ty) {
    continue;
}
```

`is_addr_exposable_ptr_ty` depends on `is_thin_ptr_ty`, and `is_thin_ptr_ty` rejects
opaque/generic-looking pointees (`has_opaque_types`, `has_aliases`, etc.) to avoid
misclassifying pointer *values* as thin.

That rule is reasonable for exposing an existing pointer value, but it is too strict for
`StackAlloc`: here we are taking `&raw const local` for a concrete stack local that already
exists in this MIR body. If the local is `Sized`, its stack-slot address is valid and should
be recorded.

Because of those two issues, the live stack allocation for the range-argument slot was never
reliably materialized in the instrumented code. Later, `Bytes::slice` created a
`Bound<&usize>` carrier that pointed into that slot. The runtime then looked up the inner
`&usize` pointee address, found only stale dead metadata for a previous occupant at the same
numeric stack address, and raised `REF_CREATE_FROM_DEAD_ALLOC`.

## Difference from `main`

This specific bug is not the same as the current bytes failures on `main`.

- On this branch before the fix, the `Bytes::slice` machine code did **not** record a live stack
  allocation for the 16-byte range-argument slot before the `Bound<&usize>` ref-create path
  used it.
- After the fix, the same function records that live 16-byte stack allocation first, and the
  reproducer no longer hits `REF_CREATE_FROM_DEAD_ALLOC`.
- On `main`, the same crashing input does not hit this dead-stack bug in `Bytes::slice`; it
  fails elsewhere with a different `TREE_BORROWS_VIOLATION`.

So the comparison is:

- `main`: still not bytes-clean overall, but not broken in this exact `Bytes::slice`
  stack-allocation way.
- this branch before the fix: regressed `StackAlloc` lowering for opaque-sized locals and made
  `Bytes::slice` recover stale dead stack metadata.

## Patch

The fix is intentionally narrow:

1. Emit `StackAlloc` for fixed-size stack locals too; only suppress synthetic `size=0`
   sentinel cases.
2. In `InstrKind::StackAlloc` lowering, allow `&raw const local` for any local that is
   `Sized` in the current typing environment, even if the local type is opaque/generic-looking.

That keeps the conservative wide-pointer handling for pointer-value exposure, but stops dropping
valid stack-slot lifetime records for concrete locals such as
`impl RangeBounds<usize>` in optimized `bytes::Bytes::slice`.

## Validation

- The original `bytes` crash reproducer now exits `0`.
- A short AFL smoke run still finds other bytes crashes/timeouts, so this fixes one false-positive
  class, not the entire bytes backlog.
