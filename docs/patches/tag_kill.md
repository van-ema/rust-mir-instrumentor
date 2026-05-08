# `__rz_tag_kill` and `__rz_tag_retain`

This note explains the holder-aware tag-lifetime hooks used for pointer/ref locals.

## Problem

Rusteze stores the current family for each pointer/ref MIR local in a hidden `u64` tag local.
Those tag values can be copied between locals:

```rust
let p = x.as_ptr();
let q = p;
```

Both `p` and `q` can carry the same tag.

A naive

```text
StorageDead(p) -> kill(tag(p))
```

is wrong, because `q` may still be live and still carry that family.

## Runtime hooks

Two hooks now model local holders explicitly:

- `__rz_tag_retain(tag)`
- `__rz_tag_kill(tag)`

Meaning:

- `retain`: one more hidden MIR tag-local now holds `tag`
- `kill`: one hidden MIR tag-local stopped holding `tag`

`__rz_tag_kill` does **not** immediately disable the alias-model node. It first decrements the
runtime holder count for that tag. Only when the last non-escaped local holder disappears does the
runtime retire the tag in the alias model.

The runtime side lives in:

- [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs)
- [runtime/src/tag_store.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/tag_store.rs)
- [runtime/src/alias_model/tree_borrows_lite.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/alias_model/tree_borrows_lite.rs)

The compiler-side insertion lives in:

- [instrument-mir/src/instrumentation.rs](/home/ubuntu/rust-mir-instrumentor/instrument-mir/src/instrumentation.rs)

## Where instrumentation inserts them

The MIR pass inserts these hooks around **hidden tag-local writes**, not around every pointer use.

For a hidden tag-local assignment:

```text
_tag_dst = <new_tag>
```

rusteze emits:

```text
Call __rz_tag_kill(_tag_dst);   // release old holder
_tag_dst = <new_tag>;
Call __rz_tag_retain(_tag_dst); // retain new holder
```

This is currently done for overwrite sites of the hidden tag locals created by
[instrument-mir/src/instrumentation.rs](/home/ubuntu/rust-mir-instrumentor/instrument-mir/src/instrumentation.rs).

Conceptually, the hidden tag local and the source-level MIR local move together:

```text
_p: &mut T          // real MIR local
_tag_p: u64         // hidden rusteze tag local
```

When `_tag_p` is overwritten, rusteze releases the old holder first and retains the new one after
the assignment.

## Why not `StorageDead`

Optimized MIR can place `StorageDead` before the last semantically relevant use through an
outstanding alias or boundary hook. We already know this for stack liveness, and the same issue
shows up for tag holders.

So the current design only releases holders on **overwrite** of the hidden tag local. That is
conservative, but it avoids prematurely dropping live families.

This is the important tradeoff:

- `StorageDead` would be more precise when it is trustworthy
- overwrite-only release is slightly leakier, but it is robust against optimized-MIR lifetime
  shortening

That is why the current implementation prefers false liveness over premature retirement.

## Example

Rust:

```rust
let p = x.as_ptr();
let q = p;
drop(p);
use_ptr(q);
```

Conceptually:

```text
tag(p) = 107
tag(q) = 107
holders(107) = 2

kill(p)  -> holders(107) = 1   // tag stays alive
use(q)   -> still valid
kill(q)  -> holders(107) = 0   // now alias model may retire 107
```

Without holder counting, killing `p` would incorrectly retire `107` while `q` was still live.

## Relation to Tree Borrows false positives

This hook exists to stop stale temporary tags from surviving forever in the alias model.

Typical shape:

```rust
let p = x.as_ptr();   // temporary shared/raw child
use_ptr(p);
x.reserve(1);         // later mutable path on the same object
```

Without holder-aware `tag_kill`, the temporary tag created for `p` can remain live even after the
hidden tag local for `p` has been overwritten. Later mutable descendants of `x` then interact with
stale alias state and can report spurious Tree-Borrows conflicts.

So `tag_kill` is not about program-visible drops. It is about keeping rusteze's hidden tag-local
lifetime aligned with the MIR storage that still carries that family.
