# Roadmap (Remaining Work Only)


## Tree Borrows precision (remaining work)
Goal:
- Improve `tb_lite` precision while preserving true-positive detection and fuzzing stability.

Open tasks:
1. Refine protector semantics.
- Model weak vs strong protector behavior explicitly.
- Model protector-end semantics for reads/writes after call boundaries.
2. Tighten access-state transitions.
- Audit transitions for nested reborrows and retag-heavy paths.
- Keep lineage repair minimal and deterministic.
3. Strengthen diagnostics.
- Keep stable violation signatures while adding better parent/root/range context.
- Distinguish confidence levels where classification is heuristic.

Validation gates:
1. `python3 scripts/run_example_tests.py` must be fully green.
2. Short AFL smoke runs must start and mutate without immediate tool aborts.
3. No regressions on known must-catch UB examples.

## Optimized-away SSA lineage
Goal:
- Preserve shared parent-child lineage when optimized MIR erases the intermediate pointer locals that used to carry it.

Open tasks:
1. Add compiler-side anchor synthesis for shared pointer expressions.
- When a pointer-valued expression such as `v.as_mut_ptr().add(i)` feeds multiple later ref/raw creations, materialize one hidden anchor local and one hidden tag local.
- Make later uses derive from that anchor instead of falling back to separate root-like tags.
2. Cover aggregate and tuple forwarding.
- If the same pointer expression populates multiple fields or return values, preserve one shared ancestor rather than reconstructing each use independently.
3. Tighten backtracking over optimized expressions.
- Extend local lineage recovery to projection-heavy `Offset` / cast / aggregate chains without inventing parentage when the source cannot be justified.
4. Keep the fallback conservative.
- If the shared ancestor cannot be reconstructed confidently, keep the current root-like fallback rather than fabricating a parent link.

Representative example:
- `paper_examples/lineage_opt_away`

Exit criteria:
1. `paper_examples/lineage_opt_away` stops being `ok` in `--release`.
2. Existing example suites remain green in default and interprocedural modes.
3. The new anchoring path preserves soundness by construction: missing lineage remains acceptable, invented lineage does not.

## Wide/Fat pointer support
Goal:
- Reduce blind spots on slices/str/dyn-trait pointer flows.

Open tasks:
1. Finish the transition from partial support to first-class DST coverage.
- Current support is strongest for slice/str wide pointers via data-pointer extraction and metadata-aware sizing.
- Remaining gap: `dyn Trait` and other DST metadata forms are still handled conservatively.
2. Better sizing for unsized accesses.
- Use slice/str metadata for dynamic access-size calculation wherever a read/write is driven by wide-pointer metadata.
- Audit helper-heavy MIR paths so size metadata survives through wrappers and temporary copies.
3. Bulk memory operation coverage.
- Improve metadata-aware sizing for copy/move/set operations on wide pointers.
- Add explicit overlap/length handling where a wide-pointer operation lowers to bulk memory movement.
4. First-class metadata plumbing.
- Preserve both the data pointer and relevant metadata across more retag/derivation/call-boundary paths instead of relying only on wrapper classification.
- Decide which runtime hooks need direct metadata operands for full DST support.
5. Regression coverage.
- Add focused examples for wide-pointer derivation, read/write, and UAF/OOB behavior.
- Keep the existing known-gap examples for slice-length OOB cases until they are upgraded to must-catch tests.

Exit criteria:
1. No routine UNKNOWN_TAG/WILD_POINTER noise on normal slice/str operations in core targets.
2. Slice/str regressions remain stable across refactors.
3. The current slice-length OOB gaps are either closed or explicitly classified as deferred non-goals.
4. `dyn Trait`/general DST handling is no longer documented as conservative-by-default.

## Stdlib instrumentation and fuzzing
Goal:
- Move from stdlib-boundary checking to selective direct instrumentation of `core` / `alloc` / `std`,
  then use that capability to fuzz stdlib-heavy code paths with Rusteze.

Why this matters:
- Current `main` detects many stdlib-mediated bugs at the application boundary through wrapper
  classification and allocator interception, but it still misses bugs that only become visible
  inside stdlib bodies.
- The rebased `feature/runtime-no-std` branch shows a viable architecture for this:
  - `runtime_abi/` for a tiny `no_std` hook surface
  - build-std aware driver plumbing
  - configurable stdlib instrumentation modes

Implementation order:
1. Port architecture, not the whole branch.
- Reuse ideas from `feature/runtime-no-std`, but do not merge it wholesale.
- First candidates to port:
  - `runtime_abi/`
  - minimal `RZ_INSTRUMENT_STDLIB` mode plumbing
  - build-std aware driver support
  - bootstrap/support-crate skipping
2. Add explicit stdlib modes.
- Introduce:
  - `RZ_INSTRUMENT_STDLIB=none|core|core_alloc|all`
- Keep `none` as the default until the other modes are proven stable.
3. Start with `core`.
- First milestone:
  - `-Z build-std=core`
  - examples still green
  - no compiler/runtime crashes
4. Then extend to `core_alloc`.
- This is the first practically useful mode for:
  - `Vec`
  - `String`
  - slice/alloc-backed operations
5. Only then enable `all`.
- `all` should still skip clearly problematic support crates at first, such as:
  - `compiler_builtins`
  - `panic_*`
  - `unwind`
  - `rustc_std_workspace_*`
  - `std_detect`

Harness plan:
1. Add a few small stdlib-focused harnesses first.
- Examples:
  - `vec_ops`
  - `string_ops`
  - `slice_ops`
- These should decode bytes into operation sequences and stress stdlib internals through public APIs.
2. After stdlib modes are stable, run a small set of real harnesses under stdlib instrumentation.
- Initial candidates:
  - `bytes`
  - `smallvec`
- Only expand further if the stdlib mode is stable enough to justify the extra build/debug cost.

Validation gates:
1. `python3 scripts/run_example_tests.py` must remain green in default mode.
2. `RZ_INTERPROC_UNSAFE_SUMMARIES=1 CARGO_INCREMENTAL=0 python3 scripts/run_example_tests.py`
   must remain green.
3. Add stdlib-specific gates incrementally:
  - `core`
  - `core_alloc`
  - `all`
4. Add at least one stdlib fuzz-smoke gate before treating stdlib instrumentation as usable.

Exit criteria:
1. `RZ_INSTRUMENT_STDLIB=core` is reproducible and green.
2. `RZ_INSTRUMENT_STDLIB=core_alloc` is reproducible and green.
3. At least one stdlib-focused harness runs under fuzzing without immediate tool breakage.
4. At least one real target (`bytes` or `smallvec`) runs successfully under stdlib instrumentation.
