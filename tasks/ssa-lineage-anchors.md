# Compiler-Side Lineage Anchors for Optimized MIR

## Context

`rusteze` now preserves provenance through memory much better than before:
- phase 1 added pointer-shadow store/load for thin raw pointer slots
- phase 2 added bytewise shadow propagation for memcpy-like copies

That closes the main "pointer moved through memory" gap. It does **not** close the
remaining "pointer never existed as a stable MIR local" gap.

The canonical remaining example is:
- `paper_examples/lineage_opt_away`

In that case, the semantic common ancestor is a pointer-valued expression such as:

```rust
let q = v.as_mut_ptr().add(i);
```

Two later borrows should be siblings under `q`. In optimized MIR, the intermediate
locals that would have carried `q`'s tag may be fused away entirely. The final
reborrow sites then no longer share a materialized parent local, and the current
instrumentation falls back to separate root-like lineage.

## Goal

Preserve shared ancestry for SSA-only pointer expressions even when optimization removes
the MIR locals that originally carried that ancestry.

Concretely:
- if several later uses semantically derive from the same pointer expression
- and that expression no longer has a stable local after optimization
- materialize one synthetic anchor local and one synthetic anchor tag
- derive all later uses from that anchor

This is a compiler-side fix. Runtime same-address repair is not enough when the common
ancestor was never materialized in the first place.

## Why Pointer Shadow Is Not Enough

Pointer shadow preserves provenance when a pointer value passes through memory.

It does not help when:
- the pointer expression stays entirely in SSA/local form
- optimization erases the temporary locals that used to carry the shared tag
- there is no memory slot from which runtime shadow metadata can be reloaded

So the remaining problem is not "pointer moved through memory and lost provenance."
It is "the shared ancestor was optimized away before instrumentation could attach a
stable tag local to it."

## Target Example

`paper_examples/lineage_opt_away`

Desired semantic shape:

```rust
let p = v.as_mut_ptr();
let q = unsafe { p.add(i) };
let r = q;
let s = q;
let r_ref: &mut u8 = unsafe { &mut *r };
let s_ref: &mut u8 = unsafe { &mut *s };
```

Desired tag shape:

```text
v_tag -> p_tag -> q_tag
                 |-> r_ref_tag
                 |-> s_ref_tag
```

Bad optimized shape today:
- no stable `p`
- no stable `q`
- two ref-creation sites each reconstruct lineage independently
- both may fall back to `parent = 0`

## Design

### 1. Normalize pointer expressions

Before emitting the tag for a ref/raw creation site, try to normalize the pointer-valued
source into a compact lineage expression:

```text
BaseLocal(v)
  -> WrapperRet(as_mut_ptr)
  -> Offset(i)
  -> Cast(...)
```

The important property is not the exact syntax. It is whether two later uses normalize to
the same pointer-valued expression with the same semantic ancestor.

### 2. Materialize a hidden anchor local

When the normalized expression:
- has no stable MIR local of its own
- and is consumed by more than one later tag-creating use

insert one synthetic local before the first use:

```text
_rz_anchor = <normalized pointer expression>;
_rz_anchor_tag = <derive once from the best available parent>;
```

All later tag-creating uses in that dominance region then derive from:
- `_rz_anchor`
- `_rz_anchor_tag`

instead of reconstructing lineage independently at each use site.

### 3. Share anchors across aggregate and tuple forwarding

The anchor must also apply when the same pointer expression flows through:
- tuple construction
- aggregate returns
- repeated field assignment

Example:

```rust
fn make_two_refs(v: &mut Vec<u8>, i: usize) -> (*mut u8, *mut u8) {
    let q = unsafe { v.as_mut_ptr().add(i) };
    (q, q)
}
```

The aggregate should be lowered from one anchor, not from two unrelated root-like tags.

### 4. Keep the fallback conservative

The anchor path must never fabricate lineage.

If normalization cannot prove that two later uses come from the same semantic expression:
- do not invent an anchor
- keep the existing root-like fallback

This preserves the repository soundness rule:
- missing metadata is acceptable
- incorrect metadata is not

## Algorithm Sketch

1. At each tag-creating site, attempt to backtrack the source pointer expression.
2. Canonicalize the recovered expression into a structural key.
3. Within the current dominance region, memoize:
   - `structural_key -> anchor_local`
   - `structural_key -> anchor_tag_local`
4. If the key is first seen:
   - emit anchor materialization once
5. If the key is seen again:
   - reuse the existing anchor locals
6. Derive all later ref/raw creations from the shared anchor tag

## Initial Scope

Keep the first implementation narrow:
- same basic block or same immediate dominance region
- raw pointer expressions composed of:
  - local base
  - known wrapper return (`as_mut_ptr`, similar pointer-derivation sites)
  - `Offset`
  - simple casts
- tuple and aggregate forwarding of those same expressions

Do not start with:
- full CFG-wide phi handling
- arbitrary joins across loops
- opaque calls
- DST/wide-pointer anchors

## Likely Implementation Points

Primary file:
- `instrument-mir/src/instrumentation.rs`

Likely touch points:
- pointer-source backtracking helpers
- ref/raw creation emission
- aggregate/tuple instrumentation
- call/wrapper derivation handling

The fix should remain compiler-side. Runtime recovery stays as a secondary mechanism for
same-address repair, not the primary way to invent missing SSA ancestry.

## Success Criteria

1. `paper_examples/lineage_opt_away` stops being `ok` in `--release`.
2. The resulting tags for the two later borrows are siblings under one shared anchor.
3. Default and interprocedural example suites remain green.
4. No new invented-parent false positives appear in the existing examples.
