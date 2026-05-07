# Call-Boundary Export Parent Design

## Context

Pointer call arguments now carry explicit boundary export state:

- caller exports an exact tag, a boundary-parent tag, and an origin through
  `__rz_push_call_arg_boundary_tag`
- callee imports it through `__rz_take_call_arg_tag`
- callee entry retags from that imported parent

This is close to Miri's `FnEntry` retag intent, but not identical.

The important semantic split is that direct refs validate/export their exact local tag, while
refs imported or recovered from a prior boundary validate/export the recorded boundary parent.
That replaces the older policy flag that asked the runtime to infer whether it should
canonicalize before validation.

## Goal

Represent two different notions explicitly:

1. **Exact local tag**
   - used for local reads, writes, and local ref/raw creation
2. **Boundary export parent**
   - used only when exporting a pointer/ref through `CallArgPush`

This should make call-boundary retagging more Miri-like:
- direct exact refs export their exact parent
- recovered refs export the live family that callee `FnEntry` retagging should inherit

## What "canonicalization" means

In this design, **canonicalization** does not mean "invent some plausible live tag for this
address".

It means:

- start from a tag that was **actually exported or imported** through a call boundary
- walk back from that exact transient tag to the nearest **live same-address family** that should
  survive helper teardown
- use that surviving family as the boundary parent

Typical safe shape:

```rust
fn helper(b: &mut BytesMut) { /* helper-heavy local reborrows */ }
helper(&mut bytes);
```

Inside `helper`, the newest exact tag on the `BytesMut` slot may be a transient child created by
local reborrows. Exporting that exact child back to the caller is wrong if it will be disabled at
call exit. Canonicalization maps that transient exact child back to the live family that the
caller should keep using after the call returns.

Important non-example:

- if a side channel exported **nothing** and returns `0`
- canonicalization must **not** recover some unrelated live same-address tag just because one
  exists locally

`0` is semantic information:

- "no export happened"

For pointer-only mut-arg writeback, that means the caller should keep its existing exact tag and
boundary parent. Turning `0` into a recovered family is not canonicalization; it is local tag
recovery, and it can import dead intermediate lineage back into the caller.

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

## Design

### 1. Add explicit boundary-export state for pointer locals

For each pointer local, track:

- `exact_tag`
- `boundary_parent_tag`
- `boundary_origin`

The first implementation uses a binary origin:

- `Exact`
- `RecoveredBoundary`

The origin can be split later into more specific classes if diagnostics or policy need to
distinguish return imports from mut-arg writeback or carrier reconstruction.

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

## Implementation Status

The current branch implements the first explicit-state slice:

- call-arg export passes `exact_tag`, `boundary_parent_tag`, and `boundary_origin`
- direct exact locals validate exact-first
- recovered boundary locals validate/export the boundary parent
- the old `CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE` policy flag is removed

The remaining open part is precision, not representation: recovered origin is still binary, and
memory round-trips depend on the existing pointer-shadow export-parent channel.
