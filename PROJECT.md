# Project Overview

This repository implements **rusteze**, a dynamic memory-safety instrumentation framework for Rust.

The system works by:
- Instrumenting **Rust MIR** during compilation to track pointer creation, propagation,
  and memory accesses
- Linking against a **runtime library** that records allocations and checks pointer use
  at runtime

The goal is to detect bugs such as:
- Out-of-bounds reads and writes
- Use-after-free
- Stale pointers caused by address reuse
- Invalid aliasing patterns (moving toward Stacked Borrows / Tree Borrows–like checks)

This is **not** a verifier or a formal model:
- Checks are dynamic
- Best-effort and conservative behavior is expected
- Missing metadata is acceptable; incorrect metadata is not

---

## Architecture (high level)

The project has two main components:

1. **Compiler pass (MIR instrumentation)**
   - Inserts runtime hooks into MIR
   - Tracks pointer lifetimes, derivation, and call boundaries
   - Must be extremely robust to generics, projections, and partial type information

2. **Runtime library**
   - Tracks allocations (stack + heap)
   - Assigns pointer tags and epochs
   - Performs runtime checks on pointer reads and writes
   - Must never crash or recurse infinitely due to its own instrumentation

3. **Code reference**

- Project design/details: `dynBorrowProposal/main.tex`
- MIR instrumentation pass entry point: `instrument-mir/src/instrumentation.rs`
- Runtime checks entry point: `runtime/src/lib.rs`
- Aliasing model implementations (pluggable): `runtime/src/alias_model/`

4. **Contraints**
- Build policy: always use `RZ_INSTRUMENT_ALL_DEPS=1`. Only `std`/`core` are treated
  as non-instrumented for unknown-call warnings and classification heuristics.
- Compile with CARGO_INCREMENTAL=0 to force building the crates.
- Soundness policy: every analysis, optimization, and instrumentation rewrite must be sound by
  default. If soundness is not established yet, keep the behavior disabled by default and gate it
  behind an explicit opt-in flag until it is proven safe.

5. **Fuzzing objective and crash policy**
- Primary objective: implement and improve `rusteze` in this repository, and use it to fuzz real-world Rust targets to find bugs/vulnerabilities.
- Quality objective: minimize false positives and avoid breaking target-program behavior due to instrumentation, while keeping detection of real memory-safety and aliasing-rule violations high.
- During fuzzing, keep violation-as-crash behavior enabled to make findings visible to AFL:
  use `RUSTEZE_FAILFAST=1` and `RZ_ABORT_ON_VIOLATION=1` by default.
- Do not silently suppress violations just to keep fuzzing running; prefer fixing root-cause
  false positives in instrumentation/runtime and keep high-confidence crashes actionable.

6. **Testing and commit gate**
- Before every commit, run the relevant test coverage for the touched code.
- Do not commit while any targeted test is failing.
- Exception: commits that touch only the paper submodule `dynBorrowProposal/` do not require the
  parent-repo example-suite gate. For those paper-only commits, rebuild the paper and verify
  `dynBorrowProposal/main.pdf` updates successfully instead.
- Before every commit, run the full example test suite, not only examples related to the touched
  code.
- Before every commit, run the full example test suite in both supported modes:
  - default:
    - `python3 scripts/run_example_tests.py`
- Do not commit while any example is failing in either mode: both suites must pass before
  committing.
- After each commit, re-run the full example test suite in both supported modes and verify both
  are still fully green on the committed revision.
