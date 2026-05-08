# TB-lite `ShadowedLocal` Design Note

## Goal

Replace the hottest remaining TB-lite recovery path,
`read_disabled_live_unique_lineage_ancestor`, with a principled state-model change.

The intended fix is **not**:

- "if a read hits `Disabled`, search for some good ancestor and rescue it"

The intended fix is:

- stop producing `Disabled` for nodes that were only superseded by later **same-lineage**
  local activity
- reserve `Disabled` for nodes that were actually invalidated by a foreign conflict,
  protector failure, or equivalent hard exclusion

This note is the design for that split.

## Problem

Today `runtime/src/alias_model/tree_borrows_lite.rs` uses:

```rust
enum TbPerm {
    Reserved { conflicted: bool },
    Active,
    Frozen,
    Disabled,
}
```

That collapses two different situations into one state:

1. **true invalidation**
   - foreign conflicting access
   - protector conflict
   - other cases where later use should remain UB

2. **local supersession**
   - a node lost its role as the currently-governing local handle because a descendant in the
     same family took over
   - no foreign conflict happened
   - later local-family activity should not have to recover from "hard dead"

The measured recovery audit shows this compression is still active in practice:

- `read_disabled_live_unique_lineage_ancestor` is the hottest remaining recovery path

So the model is still producing "fake-dead" local intermediates and then rescuing them later.

That should be fixed at the state level.

## What "shadowed" means

`ShadowedLocal` means:

- this node is no longer the currently-governing local handle
- the reason is **same-lineage local supersession**
- the node was **not** invalidated by a foreign conflict

In plain terms:

- a later local descendant took over
- the old node became outdated inside the same family
- but it did not become semantically dead in the strong "use is UB" sense

### Example

```rust
let mut x = 0;
let r1 = &mut x;
let r2 = &mut *r1;
*r2 = 1;
let y = *r1;
```

Here `r2` is a descendant of `r1`.

If TB-lite currently marks an intermediate `Unique` for `r1` as `Disabled` after the local
write through `r2`, that overstates what happened.

What really happened is:

- `r1` was superseded locally by `r2`
- not invalidated by a foreign aliasing event

That is the intended meaning of `ShadowedLocal`.

## How `ShadowedLocal` differs from `Disabled`

### `ShadowedLocal`

- same-lineage local supersession
- still part of the live local family
- should not by itself cause later local reads to be UB
- may still be relevant for overlap / ancestry / protector reasoning

### `Disabled`

- hard exclusion / invalidation
- foreign conflict, protector failure, or equivalent
- later use should still fail unless some other explicitly-modeled rule says otherwise

In short:

- `ShadowedLocal` = locally outdated
- `Disabled` = semantically dead

## Why this is better than the proposed Phase A normalization

The temporary "Phase A" idea was:

- compute an effective local head
- when a local read hits `Disabled`, allow it if a live local head still governs the range

That can work as a diagnostic stepping stone, but it encodes the fix as:

- "override `Disabled` later"

instead of:

- "stop using `Disabled` for this case"

The latter is the better design.

If a node is only locally superseded, it should not become `Disabled` in the first place.

## How Tree Borrows / Miri differ

This is where the distinction matters.

### Full Tree Borrows / Miri

Miri's Tree Borrows implementation is more precise than TB-lite:

- the model is node-centric
- node state is authoritative
- if a node is `Disabled`, that means something real in the model

In particular, Miri's `child_read` transition treats `Disabled` as UB.

That means Miri does **not** do our proposed Phase A normalization:

- it does not say "a local read is okay because an ancestor still looks good"

So if TB-lite needs that kind of rescue, the honest conclusion is:

- TB-lite compressed too much state
- not that Miri intended this behavior directly

### Why `ShadowedLocal` is still aligned with the design direction

Tree Borrows is organized around:

- local vs foreign access
- reservation vs activation
- the idea that same-lineage local behavior is different from foreign conflict

`ShadowedLocal` is a TB-lite-specific implementation state meant to preserve that distinction
under a compressed runtime representation.

So:

- it is **not** a Miri state
- it is a TB-lite approximation device

But unlike Phase A, it keeps the approximation in the **producer of the state**, not in a
later read-side override.

That is a better fit to the model.

## Planned state change

Change:

```rust
enum TbPerm {
    Reserved { conflicted: bool },
    Active,
    Frozen,
    Disabled,
}
```

to:

```rust
enum TbPerm {
    Reserved { conflicted: bool },
    Active,
    Frozen,
    ShadowedLocal,
    Disabled,
}
```

## First producer to change

The primary producer is the local-write branch:

```rust
(AliasAccessKind::Write, true, _, _)
    if child_unique_ref_ancestor && matches!(tmeta.kind, PtrKind::RefMut) =>
```

Today that branch can mark the covered ancestor `Unique` as:

```rust
TbPerm::Disabled
```

That should instead become:

```rust
TbPerm::ShadowedLocal
```

when the disabling event is just same-lineage local supersession.

This is the narrowest and most important first patch.

## Minimal semantics for the new state

First implementation should stay narrow.

### Local read

```rust
(AliasAccessKind::Read, true, TbPerm::ShadowedLocal, _) => TbPerm::ShadowedLocal
```

Meaning:

- a local read through or beneath a locally-shadowed node is allowed

This should be enough to drive `read_disabled_live_unique_lineage_ancestor` toward zero.

### Local write

Start conservatively:

- do not introduce aggressive write reactivation in the first patch
- keep the write behavior narrow and validate with suites before expanding it

### Foreign access

Also stay conservative:

- foreign conflict should still be able to disable the node
- `ShadowedLocal` must not become a permissive bypass for real alias violations

## Expected effect

After the first `ShadowedLocal` patch:

1. same-lineage read-after-local-supersession stops relying on recovery
2. `read_disabled_live_unique_lineage_ancestor` should go to zero or near-zero
3. `Disabled` becomes closer to "hard dead" again

Then the read recovery branch can be removed cleanly.

## Recommended implementation order

1. add `TbPerm::ShadowedLocal`
2. change the local-write supersession producer from `Disabled` to `ShadowedLocal`
3. add minimal read semantics for `ShadowedLocal`
4. rerun:
   - full default example suite
   - full interproc example suite
5. re-measure live recovery predicates
6. remove `read_disabled_live_unique_lineage_ancestor` if it is dead

## Non-goals

- Do not claim that `ShadowedLocal` exists in Miri.
- Do not describe this as "the Tree Borrows rule".
- Do not broaden local-write behavior in the same patch unless the tests clearly justify it.
- Do not use `ShadowedLocal` to excuse real foreign conflicts or protector failures.
