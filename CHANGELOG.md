# Changelog

## Unreleased

- Tree Borrows (lite): added call-boundary protector plumbing.
  - Runtime now notifies the active alias model when call-argument parent tags are consumed (`__rz_take_call_arg_tag`) and when instrumented callees exit (`__rz_exit_fn`).
  - Instrumentation now inserts `FnExit` hooks at each MIR `Return` terminator.
- Tree Borrows (lite): implemented protector checks for:
  - overlapping writes through non-protected tags (`TB_LITE_PROTECTOR_CONFLICT`)
  - heap deallocation while a protected tag is still active (`TB_LITE_PROTECTOR_DEALLOC`)
- Tree Borrows (lite): introduced explicit per-tag permission states:
  - `Reserved`, `Active`, `Frozen`, `Disabled`
  - state transitions now drive invalidation instead of a pure `alive` bit
  - unique/raw-mut writes activate the writer tag; conflicting branches become `Disabled`
  - unique reads freeze shared/raw-const descendants and disable unique/raw-mut descendants
- Examples: added protector-focused TB micro tests:
  - `tb_miri_micro::protected_raw_write`
  - `tb_miri_micro::protected_dealloc`
  with model-specific expected outputs.
- Tree Borrows (lite): decoupled `tb_lite` runtime checks from SB-lite delegation, so `RZ_ALIAS_MODEL=tb_lite` now executes its own alias decisions.
- Tree Borrows (lite): track raw tags in TB state and use original tag when available, enabling descendant invalidation checks to apply to raw accesses.
- Tree Borrows (lite): add parent-read invalidation approximation for unique ancestors (`TB_LITE_INVALIDATED` on subsequent conflicting raw writes).
- Tests: `scripts/run_example_tests.py` now supports model-specific expectation files (`expected.<bin>.<model>.rz`, `expected.<model>.rz`) and `EXAMPLE_FILTER`.
- Examples: add `examples/tb_miri_micro` (Miri-inspired TB micro-suite) with per-model expectations.
- SB-lite false-positive fix: allow parent `&mut` reactivation on reads when blocked only by newer unique descendant reborrows (best-effort NLL/liveness approximation). This addresses safe patterns like serde-style field reborrows followed by parent reads.
- SB-lite false-positive fix: read checks now ignore unique blockers from a different root tag lineage to avoid cross-lineage conflicts when multiple independent `&mut` args share the same coarse allocation bucket.
- SB-lite false-positive fix: when a parent `&mut` tag is missing from stack (due to reborrow truncation), read checks now reactivate it if the remaining stack contains only unique descendants of that parent.
- Examples: add `sb_lite_parent_read_after_field_reborrow` and mark expected result as `ok` to lock in the fixed behavior.
- Runtime false-positive fix: in no-allocation branches, allow untracked stack `&/&mut` (epoch unknown) instead of reporting `WILD_POINTER` when addresses are within stack-hint range.
- Instrumentation: handle wide/fat pointers (`&[T]`, `&str`, `dyn Trait`) by extracting a thin data pointer (`*const ()`/`*mut ()`) before `PointerExposeProvenance`, so tags/epochs are keyed by the data address.
- Instrumentation: treat wide pointer locals as tag-relevant for deref read/write and raw-root synthesis so lineage isn’t dropped to tag=0 before a thin data pointer is extracted.
- Examples: add wide-pointer micro-examples (`wide_ptr_slice_uaf_read`, `wide_ptr_slice_uaf_write`, `wide_ptr_raw_slice_cast_ok`, `wide_ptr_raw_str_cast_ok`).
- SB-lite: invalidate on new unique reborrow (truncate/clear stack) and tighten access checks to require the accessing tag to be present and not blocked by a newer unique borrow.
- SB-lite: raw pointers now check against the nearest reference ancestor (SB-style), with a heuristic to allow raw writes when only shared reborrows are above the unique ancestor.
- Instrumentation: avoid inserting `RawRoot` for pointer locals that already have a real tag source to prevent tag overwrite in control-flow orderings.
- Instrumentation: cast/transmute to raw pointers now creates a fresh raw tag with parent lineage; added call classifiers for `ptr::from_ref`, `ptr::from_mut`, and `mem::transmute`.
- Runtime/Instrumentation: return-tag recovery now falls back to synthesizing a raw tag (`__rz_take_ret_tag_or_root`) to avoid `UNKNOWN_TAG` when a callee doesn’t push a return tag (fixes `medium_smallvec_driver`).
- Examples: add SB-lite micro suite (`sb_lite_two_mut`, `sb_lite_shared_then_write`, `sb_lite_shared_after_unique_read`, `sb_lite_raw_write_after_unique`, `sb_lite_raw_read_after_unique`, `sb_lite_raw_cast_from_ref`, `sb_lite_raw_from_ref_fn`, `sb_lite_raw_transmute`) with expectations.
- Tests: `scripts/run_example_tests.py` (all examples passing).

## Since aa1494c6dcb61e77aed2d645cf1391ab691b50e9

- Serde/memchr crash fix: resolved `UNKNOWN_TAG`/`STALE_POINTER_EPOCH_MISMATCH` seen in `afl_serde_json_driver` reproducer by:
  - Instrumentation: treat `BinOp::Offset` as pointer derivation (`PtrDerive`) so offset-derived pointers do not remain tag `0`.
  - Instrumentation: synthesize `RawRoot` for pointer casts from non-pointer sources (including transmute/exposed-provenance paths) when lineage cannot be propagated.
  - Runtime: for coarse `*_allow_untagged` hooks only, relax stack epoch-mismatch rejection to avoid false stale reports from unknown-call instrumentation on stack references.

## Since b9087a6b61b074410c69f59405489fde9ea7afe4

- Runtime: record wide-pointer metadata length in tags and enforce bounds against it for reads/writes.
- Instrumentation: compute access sizes for wide-pointer derefs using `PtrMetadata` (slice/str length).

## Since 487b00526d7fe9755d37f4109b01c85af1c23dfd

- Instrumentation: compute deref-access addresses with projection offsets (field/index/subslice) so reads/writes use the actual element address.
- Examples: kept wide-pointer slice-index OOB demos as `ok` because Rust inserts bounds-check panics before runtime hooks execute.
