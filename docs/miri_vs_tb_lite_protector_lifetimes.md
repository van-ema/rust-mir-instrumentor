# Miri vs TB-lite Protector Lifetimes

This note describes one important difference between Miri's Tree Borrows interpreter and rusteze's
TB-lite runtime model.

## The core difference

Miri stores the exact tag in each pointer value. When a function returns, Miri can remove the
function's protector without making the tag itself disappear or become disabled. If no Rust value
still carries that tag, the tag is simply unreachable by later execution.

Rusteze cannot rely only on pointer values. TB-lite also has runtime shadow maps and call-boundary
side channels. A tag from a dead local can still be recoverable by address or by boundary metadata
unless the runtime marks it as no longer usable.

That is why TB-lite has a middle state:

```text
ShadowedLocal
```

`ShadowedLocal` means the old local handle is obsolete, but live descendants still need it in their
parent chain.

## Example

Simplified code:

```rust
fn outer(x: &mut Input) {
    inner(x);
}

fn inner(y: &mut Input) {
    y.next();
}
```

Miri's logical tree:

```text
A
`- B   outer's protected &mut Input
   `- C   inner's protected &mut Input
```

When `inner` exits, Miri removes `C`'s protector. It does not disable `B` or `C` just because the
protector scope ended:

```text
A
`- B   still live
   `- C   unprotected
```

If no pointer value carries `C`, later code cannot accidentally use `C`.

TB-lite sees the same shape through hooks:

```text
B protected
`- C protected
   `- D live descendant exported or carried forward
```

If TB-lite simply leaves every old tag active, stale local tags can be recovered later from shadow
metadata. If it disables every non-exported protected tag, it can create a bad shape:

```text
B Disabled
`- C Active
   `- D Active
```

Then a later access through `D` fails with `TB_LITE_DISABLED_ANCESTOR`.

The correct TB-lite shape is:

```text
B ShadowedLocal
`- C Active
   `- D Active
```

This keeps the lineage intact while preventing the old local handle from acting as current
authority.

## Rule

At protector exit:

- if the exact protected tag escaped, keep or re-enable it;
- if the tag did not escape and has no live descendant, it can be disabled;
- if the tag is an older same-slot protected `&mut` ancestor and a live same-slot descendant still
  depends on it, mark it `ShadowedLocal`, not `Disabled`.

This matches the Miri behavior we need: removing a protector is not the same as killing every tag in
the lineage.
