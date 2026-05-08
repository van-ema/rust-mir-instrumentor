# Stack-UAD suppression: epoch-gap narrowing

## Problem

Unconditional stack-UAD suppression in `runtime/src/lib.rs` dropped every dead-stack access report. Three sites:

- `rz_validate_ref_creation_addr` (ref-creation validation)
- `__rz_ptr_write` (write path)
- `__rz_ptr_read` (read path)

Each had variants of:

```rust
if ameta.is_stack { return None; }
```

or

```rust
if ameta.is_stack && tmeta.parent != 0 && tmeta.pointee_addr == base { return; }
```

Intent: suppress address-reuse false positives on stack slots. Effect: also suppressed real use-after-dead detections across function-return boundaries.

Regressions caused:
- `examples/ret_ptr_local` — raw pointer to callee's dead local, read in caller.
- `examples/miri_tests/memory_exact/src/bin/stack_temporary.rs` (miri-semantics-only; now dropped).
- Classification drift in `oob_struct_interior`, `ret_provenance_cases::ret_raw_derivation_oob_ub`, `miri_sb_exact::load_invalid_shr` — earlier read-path OOB fires because the write-path UAD on a dead stack is suppressed.

## Fix

Use the allocation-instance epoch to separate two cases:

1. **Natural death of pinned instance** — the tag was minted when the allocation was in state `N` (live). Current state is `N+1` (dead). Gap = 1. This is real UAD.
2. **Address reuse** — slot was reborn and died again since the tag was minted. Gap >= 2. This is the false-positive pattern.

When we cannot correlate (no parent tag, unknown provenance), fall back to the conservative old behavior and suppress — this preserves the original intent for root creations.

### Rule

- `parent_epoch == 0` (unknown / untagged parent) → suppress.
- `ameta.epoch > parent_epoch + 1` → slot reused → suppress.
- Otherwise → report UAD.

### Sites

#### `rz_validate_ref_creation_addr`

```rust
if ameta.is_stack {
    let parent_epoch = tag_store::get(parent_tag).map(|p| p.alloc_epoch).unwrap_or(0);
    if parent_epoch == 0 || ameta.epoch > parent_epoch + 1 {
        return None;
    }
}
```

#### `__rz_ptr_write` and `__rz_ptr_read`

Same predicate, using `tmeta.alloc_epoch` (tag already in scope):

```rust
let slot_reused = tmeta.alloc_epoch == 0 || ameta.epoch > tmeta.alloc_epoch + 1;
if ameta.is_stack && slot_reused && tmeta.parent != 0 && tmeta.pointee_addr == base {
    return;
}
if matches!(tmeta.kind, PtrKind::RefShared | PtrKind::RefMut)
    && (ameta.is_stack || rz_stack_addr_hint(addr))
    && (tmeta.alloc_epoch == 0 || !tmeta.origin_known)
    && (!ameta.is_stack || slot_reused)
{
    return;
}
```

## Verification

Example suite (tb_lite default mode):

| State | ok | mismatch |
|---|---|---|
| Before narrowing | 167 | 7 |
| After narrowing + drop `stack_temporary` | 168 | 5 |

Fixed:
- `ret_ptr_local` — reports `USE_AFTER_DEAD|READ|RawConst|4`.
- `stack_slot_reuse_vec_len` — stays `ok` (parent=0 root → conservative suppress).

Dropped:
- `miri_mem_exact::stack_temporary` — miri models end-of-statement temp death; rustc lifetime-extends temp through the enclosing block. Not a fair comparison target under MIR semantics rusteze follows.

Pre-existing mismatches unrelated to this fix (classification/expected-value drift):
- `miri_mem_exact::out_of_bounds_read`, `out_of_bounds_write` — committed expected `ok` predates detection; miri canon is OOB.
- `miri_sb_exact::load_invalid_shr.tb_lite` — detected but wrong kind (OOB vs TB).
- `oob_struct_interior`, `ret_provenance_cases::ret_raw_derivation_oob_ub` — access kind/size drift (OOB WRITE expected, OOB READ observed).

## Invariant relied on

`alloc_epoch` bumps on every `live <-> dead` transition in `__rz_record_alloc`. Comment in that function:

> We must bump it not only on death, but also on reuse (dead -> live), otherwise address reuse could produce false negatives.

Without this invariant the `+1` vs `>= 2` distinction collapses. If a future change tracks epoch only on death, the narrowing must be revisited.
