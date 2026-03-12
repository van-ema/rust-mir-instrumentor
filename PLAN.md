# Roadmap (Remaining Work Only)

## Current focus
- Phase 4 (Tree Borrows precision): in progress.
- Phase 5 (ecosystem coverage): in progress.
- Phase 6 (fuzzing + evaluation): in progress.
- Phase 7 (wide/fat pointers): in progress.
- Phase 8 (runtime performance): in progress.

## Phase 4 - Tree Borrows precision (remaining work)
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

## Phase 5 - Bigger crates and ecosystem coverage
Goal:
- Expand to maintained, popular crates with significant `unsafe` usage.

Open tasks:
1. Expand target matrix.
- Keep and maintain current targets: `bytes`, `smallvec`, `serde_json`, `toml`, `base64`, `uuid`, `itoa`, `quick_xml`, `simd_json`, `zip`, `rkyv`, `hyper`.
- Add additional high-value targets incrementally with pinned versions.
2. Stabilize build/fuzz onboarding per target.
- Ensure each target has build + repro + fuzz commands documented and reproducible.
3. Keep false positives low.
- Triage every new crash as real bug vs modeling issue before broad suppressions.

## Phase 6 - Fuzzing and evaluation
Goal:
- Produce defensible results (engineering and research quality).

Open tasks:
1. Crash triage loop.
- Reproduce with `scripts/afl_repro.sh`.
- Minimize with `afl-tmin`.
- Classify and convert stable findings into regressions or documented repros.
2. Metrics and reporting.
- Collect overhead (`baseline` vs `rusteze` vs `ASan`, optional `Miri`) using `scripts/bench_overhead.py`.
- Track unique signature counts and violations/hour per target.
3. Reproducibility.
- Keep Docker/native workflows aligned.
- Keep reports versioned under `reports/` with target, profile, and model metadata.

## Phase 7 - Wide/Fat pointer support
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

## Phase 8 - Runtime performance
Goal:
- Reduce runtime overhead on parser-heavy ecosystem crates while preserving dynamic-checking behavior.

Open tasks:
1. Tune remaining high-overhead targets.
- Focus on `toml`, `zip`, and other parser-heavy outliers.
- Use profiling split to separate alias-model vs alloc-check costs.
2. Improve interprocedural unsafe-sensitive pruning.
- Keep the current two-phase summary pipeline (`analyze-only` -> merge -> instrumented build) sound and conservative.
- Tighten backward call-boundary transfer so merged summaries remove more irrelevant creation/access hooks across dependencies.
- Measure whole-build totals across dependencies, not only final harness crates.
3. Validate and regressions.
- Benchmark with and without alias model (`tb_lite` and `none`) using `scripts/bench_overhead.py`.
- Ensure functional tests/fuzz smoke still pass after each optimization step.
