# Call-Only Shared-Ref Tag Elision

## Context

The current short-term fix for `bytes::BytesMut::unsplit` is to retire the caller-side temp ref
holder immediately after the helper call returns:

```text
_tmp = &(*self)
call is_empty(move _tmp)
__rz_tag_kill(tag(_tmp))
*self = other
```

That fixes the false positive without weakening Tree Borrows, but it still pays the full cost of:

- creating a fresh shared ref tag for `_tmp`
- retaining a local holder for that tag
- exporting the tag at the call boundary
- killing the holder after return

For pure helper calls like `is_empty`, this is more work than we need.

## Optimization idea

Do not emit a fresh shared-ref tag at all for a narrow class of call-only temps.

Target MIR shape:

```text
_tmp = &(*base)
call helper(move _tmp)
// `_tmp` has no other non-def uses
```

Representative source shape:

```rust
if self.is_empty() {
    *self = other;
}
```

## Proposed eligibility rule

Only elide the tag when **all** of these hold:

1. `_tmp` is a shared ref local defined exactly once by `Rvalue::Ref(_, BorrowKind::Shared, src)`.
2. `_tmp` has no non-def uses outside the single call terminator.
3. The callee summary for that argument says:
   - no direct sink
   - no unknown escape
   - not forwarded to return
   - no write capability that would matter for alias checks
4. The temp is not used in any boundary-recovery / wrapper / carrier path.
5. The base source is already tracked strongly enough that skipping the child cannot hide a real
   aliasing violation.

In practice, the first useful extension is:

- reuse the existing `compute_summary_elidable_shared_call_ref_locals` machinery
- extend it to simple reborrow temps of the form `&(*base)` used only in one pure helper call

## Required semantic proof

This optimization is only sound if the skipped shared child is observationally irrelevant.

That means the callee must not:

- store or return the ref
- create descendants whose behavior matters after the call
- trigger protector-relevant state that must outlive the call
- use the child to justify a later alias conflict in the caller

If any of those can happen, we must still materialize the child tag and only rely on the
short-term post-call `TagKill`.

## Implementation sketch

1. Extend `compute_summary_elidable_shared_call_ref_locals` to recognize:
   - `Rvalue::Ref(_, BorrowKind::Shared, src_place)` where `src_place` is `(*base)` or another
     trivially safe reborrow source
2. Reuse the existing local-use stats requirement:
   - exactly one meaningful use, at the call terminator
3. Require a safe summary for the callee arg, not just "no unknown boundary"
4. In `scan_statement`, when the local is in that eligible set:
   - skip `InstrKind::Ref`
   - skip hidden tag-local allocation for that temp if it has no other instrumentation needs
   - skip `CallArgPush` for that temp
5. Keep the current post-call `TagKill` path as the fallback for all non-proven cases

## Why this is not the first patch

The short-term `TagKill` patch is low risk because it preserves the full alias-model behavior
during the call and only fixes the stale local-holder lifetime afterward.

This optimization is higher risk because it changes the call-time semantics by removing the child
tag entirely. It should stay as a follow-up once the summary conditions and TB proof are explicit.

## TODO

- [x] Keep the post-call `TagKill` fallback for non-proven cases.
  This preserves call-time alias-model behavior and only fixes stale local-holder lifetime after
  the helper returns.

- [ ] Extend `compute_summary_elidable_shared_call_ref_locals`.
  Recognize simple call-only shared reborrow temps such as `&(*base)` when they have one
  meaningful use at the call terminator.

- [ ] Require a safe callee-argument summary.
  The callee must not store, return, escape, write through, or create relevant descendants from
  the temporary shared ref.

- [ ] Skip tag materialization only for proven call-only temps.
  Suppress `InstrKind::Ref`, hidden tag-local allocation, and `CallArgPush` for eligible temps.

- [ ] Add targeted tests for `BytesMut::unsplit`-style helpers.
  Include both a pure-helper pass case and negative cases where the temp escapes or is returned.

- [ ] Run the full default and interproc example suites before committing any implementation.
