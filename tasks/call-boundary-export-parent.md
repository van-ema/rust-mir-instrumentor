# Call-Boundary Export Parent Design

## Context

Current short-term behavior for pointer call arguments is:

- caller exports one tag through `__rz_push_call_arg_tag`
- callee imports it through `__rz_take_call_arg_tag`
- callee entry retags from that imported parent

This is close to Miri's `FnEntry` retag intent, but not identical.

The main remaining gap is that rusteze currently exports the caller local's **exact current tag**
for most pointer args. That is wrong for refs that were already imported/recovered from a prior
call boundary, because the current exact tag can be a transient child that is already invalidated
while the correct same-address family is still live.

The current short-term fix therefore uses
`CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE` for recovered boundary refs.

That patch is intentionally narrow, but it is still a policy flag rather than a principled
representation of boundary state.

## Goal

Represent two different notions explicitly:

1. **Exact local tag**
   - used for local reads, writes, and local ref/raw creation
2. **Boundary export parent**
   - used only when exporting a pointer/ref through `CallArgPush`

This should make call-boundary retagging more Miri-like:
- direct exact refs export their exact parent
- recovered refs export the live family that callee `FnEntry` retagging should inherit

## Why the current design is insufficient

Today, one local tag is reused for two different jobs:

- "what tag describes this local right now?"
- "what parent should the next callee entry retag from?"

Those answers are often the same for direct refs, but they diverge after:

- `RetTake`
- mut-arg return/writeback recovery
- by-value carrier import/reconstruction
- helper-heavy wrapper paths that create transient local ref children

Representative safe shape:

```rust
let s = bytes.as_ref();
eq_slice(s, other);
```

Conceptually:

- `s` is imported from a prior call boundary
- later helper/local instrumentation may create a transient exact child for `s`
- the next call should still retag from the live same-address boundary family, not that transient
  child

## Long-term design

### 1. Add explicit boundary-export state for pointer locals

For each pointer local, track:

- `exact_tag`
- `boundary_parent_tag`
- optional `boundary_origin`

Suggested origin classes:

- `Exact`
- `RecoveredReturn`
- `RecoveredMutArg`
- `RecoveredCarrier`

The key semantic rule:

- `Exact` exports validate exact-first
- `Recovered*` exports canonicalize to the live same-address family first

### 2. Update the export-parent channel at the right creation sites

Direct exact refs:

- `Ref`
- `RetRoot { is_ref: true }`
- `ArgRetag`

should set:

- `boundary_parent_tag = exact_tag`
- `boundary_origin = Exact`

Recovered refs:

- `RetTake`
- pointer-only mut-arg writeback recovery
- later carrier-specific recovered-ref import paths

should set:

- `boundary_parent_tag = imported live family`
- `boundary_origin = Recovered*`

### 3. Propagate export-parent through simple forwarding

Safe forwarding should preserve boundary origin:

- `Use(Copy/Move)`
- pointer casts / transmute forwarding
- plain local-to-local wrapper forwarding

Fresh reborrows should not blindly inherit recovered export-parent if they represent a real new
source-level exact child.

That distinction is why this design must treat "copy/forward" differently from "fresh ref
creation".

### 4. Decide whether export-parent must survive memory round-trips

If recovered refs are stored to memory and later reloaded, local-only export-parent state is not
enough.

Long-term options:

1. extend pointer shadow to store an export-parent channel
2. accept local-only precision and keep some runtime recovery/canonicalization

Option 1 is more principled but has higher runtime and instrumentation complexity.

## Expected benefits

- removes ad hoc callsite flag decisions
- makes call-boundary behavior match semantic origin rather than MIR accidents
- reduces false positives on helper-heavy shared-ref flows (`bytes`, wrapper-heavy APIs)
- avoids always-canonicalize behavior that could hide real direct-ref violations

## Risks / open questions

1. **Fresh reborrows from recovered refs**
   - need a clear rule for when a new local should become `Exact` again

2. **Memory round-trips**
   - if boundary-parent is local-only, some recovered-state precision will still be lost

3. **Carrier/wrapper interactions**
   - `Option<&T>`, tuples, slices, and other scalar-pair wrappers may need matching origin rules

4. **Runtime metadata growth**
   - if we push this into shadow/runtime state, it becomes another hot-path channel

## Short-term status

The short-term patch in the current branch does **not** implement this full design.

It does only this:

- mark pointer locals recovered via `RetTake` / pointer-only mut-arg writeback
- recognize simple forwarded copies/transmutes of those locals
- set `CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE` for those exports

That is enough to fix the current `bytes` false-positive class while keeping the blast radius
small.
