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

## TODO

- [x] Implement the first explicit-state slice.
  Commit `a2d01f4` on `codex/call-boundary-export-parent`.

- [x] Lower `CallArgPush` to `__rz_push_call_arg_boundary_tag(...)`.

- [x] Pass `exact_tag`, `boundary_parent_tag`, and `boundary_origin` explicitly from the caller.

- [x] Validate direct exact locals exact-first, then export the canonical call parent.

- [x] Validate/export recovered boundary locals through the recorded boundary parent instead of a
  transient exact child.

- [x] Remove `CALL_ARG_FLAG_CANONICALIZE_BEFORE_VALIDATE`.

- [x] Remove `CALL_ARG_FLAG_USE_EXPORT_PARENT`; boundary parent is now an explicit operand.

- [x] Delete the MIR-shape helpers that guessed canonicalize-before-validate policy at call sites.

- [x] Validate commit `a2d01f4` with the full gates:
  default, interproc, post-commit default, and post-commit interproc suites were green.

- [ ] Split recovered origin if a concrete case needs it.
  `boundary_origin` is currently binary: `Exact` or `RecoveredBoundary`. If future behavior needs
  finer policy or diagnostics, split `RecoveredBoundary` into `RecoveredReturn`,
  `RecoveredMutArg`, and `RecoveredCarrier`.

- [ ] Audit fresh reborrow origin reset with targeted examples.
  Fresh source-level reborrows should become `Exact`, while forwarding should preserve recovered
  boundary origin. Cover returned-ref forwarding, returned-ref fresh reborrow, mut-arg writeback
  forwarding, and carrier-import forwarding/reborrow.

- [ ] Measure recovered-ref memory round-trips.
  Pointer shadow already stores `export_parent` and `export_parent_recovered`; add targeted
  store/reload tests to verify whether that is enough before widening shadow metadata.

- [ ] Narrow or remove residual `canonical_call_arg_tag(...)` only if it becomes redundant.
  It remains useful runtime hardening for stale or missing metadata; delete it only after traces
  prove caller-side boundary parents are precise enough without it.
