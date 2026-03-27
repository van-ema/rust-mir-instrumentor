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

## Wide/Fat pointer support
Goal:
- Reduce blind spots on slices/str/dyn-trait pointer flows.

Open tasks:
1. Better sizing for unsized accesses.
- Use slice/str metadata for dynamic access-size calculation where available.
2. Bulk memory operation coverage.
- Improve metadata-aware sizing for copy/move/set operations on wide pointers.
3. Regression coverage.
- Add focused examples for wide-pointer derivation, read/write, and UAF/OOB behavior.

Exit criteria:
1. No routine UNKNOWN_TAG/WILD_POINTER noise on normal slice/str operations in core targets.
2. Wide-pointer regression examples remain stable across refactors.

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
