# Bytes TB-lite Boundary Survivor Gap

## Summary

The remaining `bytes` false positives after `ac29b80` are not new semantic aliasing bugs in
`bytes`. They are a remaining TB-lite call-boundary retirement bug:

- a callee-local exact `&mut` tag is retired as `Disabled`
- even though a same-lineage descendant was already exported back to the caller
- the next caller-visible reborrow then fails with `TB_LITE_DISABLED_ANCESTOR`

This is the same conceptual issue as the previous `ShadowedLocal` patch, but a different producer.

The missing distinction is:

- **transport storage**: temporary runtime side-channel entries used by caller `take_*` hooks
- **boundary survivor fact**: the semantic statement that "this lineage survives the call boundary"

TB-lite still derives the second from the first in a few places, and that is too brittle.

## Reduced example

```rust
fn helper(dst: &mut [u8; 4]) {
    let tail = &mut dst[1..];
    tail[0] = 7;
}

fn caller(x: &mut [u8; 4]) {
    helper(x);
    let again = &mut *x;
    again[0] = 1;
}
```

Desired semantics:

- `x` at helper entry gets protected exact tag `U0`
- `tail` gets descendant `U1`
- the write through `U1` makes `U1` the governing local descendant
- after return, caller-visible lineage is still `U1`'s family
- `U0` is locally outdated, not hard dead
- `again` should be allowed

The old false positive is:

```text
U0 Disabled
└─ U1 Active
   └─ U2 Reserved
```

where `U2` is the next caller-visible reborrow. The ancestor check sees `U0 = Disabled` and
reports UB, even though `U1` already survived the boundary.

Desired TB-lite state:

```text
U0 ShadowedLocal
└─ U1 Active
   └─ U2 Reserved
```

## What happens in the current runtime

The `bytes` traces show this shape:

- protected exact unique: `73`
- active same-lineage descendant exported by mut-arg-ret: `82`
- next caller-visible reborrow: `97 parent=82`
- failure:

```text
READ via tag=97
reason=TB_LITE_DISABLED_ANCESTOR ancestor_tag=73
```

The representative plain harness accepts the same input, so this is a rusteze false positive.

## MIR / instrumentation shape

The relevant MIR-level sequence is:

1. callee entry:

```text
CallArgPush / ArgRetag
```

This creates the protected exact callee-local `&mut` tag.

2. local descendant reborrow:

```text
PtrDerive(is_ref = true) / ref creation
```

This creates a child tag for the derived local view.

3. callee exports the surviving family for the `&mut` argument:

```text
MutArgRetPush
MutArgRetLeafPush   // if the carrier has ref/raw leaves
```

4. callee exit:

```text
FnExit -> __rz_exit_fn(callee_id)
```

5. local retirement around return:

```text
TagKill / TagLocalKill
```

6. caller imports the surviving family:

```text
MutArgRetTake / MutArgRetTakePtrOnly
MutArgRetLeafTake
```

The bug is that steps 4 and 5 still decide "should this exact local become hard dead?" by looking
at transport state instead of a dedicated survivor fact.

## Internal runtime sequence

Relevant hooks:

- mut-arg-ret export:
  - `__rz_push_mut_arg_ret_tag`
  - `__rz_push_mut_arg_ret_leaf_shadow`
- callee exit:
  - `__rz_exit_fn`
- exact local retirement:
  - `tb_lite_on_tag_killed`
- caller import:
  - `__rz_take_mut_arg_ret_tag`
  - `__rz_take_mut_arg_ret_tag_or_zero`
  - `__rz_take_mut_arg_ret_leaf_shadow`

Before the patch, TB-lite recovers surviving descendants by scanning runtime side-channel maps such
as:

- `mut_arg_ret_tags`
- `mut_arg_ret_leaf_shadows`
- `ret_tags`
- `ret_leaf_shadows`

That is the wrong abstraction boundary. Those maps are caller/callee transport buffers, not the
authoritative semantic record of which lineages survive.

## Why the current state model fails

The remaining gap is:

- a same-lineage descendant has already been exported
- but later protector-end retirement or exact-local `TagKill` still disables the older exact tag
- the model records "hard dead" instead of "locally superseded"

That is the same compression bug as before, just through another producer.

## Principled patch

Introduce an explicit **boundary survivor set** in the runtime:

- keyed by `(thread_id, callee_id)`
- stores exported survivor tags
- populated at export time:
  - `__rz_push_ret_tag`
  - `__rz_push_ret_leaf_shadow`
  - `__rz_validate_ret_tag`
  - `__rz_push_mut_arg_ret_tag`
  - `__rz_push_mut_arg_ret_leaf_shadow`

Then:

1. `tb_lite_on_call_exit`
   - consults the survivor set instead of transport maps
   - if a protected exact unique has a same-lineage exported descendant, retire it as
     `ShadowedLocal`

2. `tb_lite_on_tag_killed`
   - if the killed exact unique still has a boundary-visible same-lineage descendant,
     retire it as `ShadowedLocal`
   - otherwise keep the existing `Disabled` retirement

3. caller `take_*` hooks
   - clear the callee's survivor set after return transport is consumed

## Why this is aligned with Miri / Tree Borrows

Miri does not use transport maps to infer semantic survival. It keeps the lineage and per-location
permission state directly.

TB-lite is more compressed, so we need an explicit runtime fact for:

- "this descendant survives the call boundary"

Using a dedicated survivor set is closer to the real semantics than:

- name-based allowances
- caller-side recovery hatches
- or continued reliance on temporary transport buffers

## Expected effect

After the patch:

- the remaining `bytes` crash bucket should stop reporting `TB_LITE_DISABLED_ANCESTOR`
- old exact callee-local uniques with exported descendants become `ShadowedLocal`
- `Disabled` becomes narrower again: true hard exclusion, not local post-call supersession
