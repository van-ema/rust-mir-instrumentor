Roadmap

Current status
	•	Phase 0 (harness): completed
	•	Phase 1 (call boundaries + std/core classification): completed (bytes/smallvec debug+release stable)
	•	Phase 2 (alloc/realloc epochs): completed
	•	Phase 3 (SB-lite): completed (micro-suite + example tests passing)
	•	Phase 6 (Fuzzing + Evaluation): in progress (AFL++ harness + Docker workflow; bytes fuzzing started)
Next step
	•	Phase 4 (Tree Borrows) for higher precision (reduce SB-lite false positives)
	•	In parallel: continue Phase 6 to turn fuzz findings into minimized, reproducible, triaged reports

⸻

Phase 0 — Lock in a Repeatable Test Harness (COMPLETED)

Goal
	•	Make failures reproducible, classifiable, and stable before adding new semantics.

Tasks
	•	Create a script: scripts/run_harness.sh
	•	The script should:
		1.	Run flaky micro-examples (e.g. memset_u8_dynamic) 200–1000 times
		2.	Run a small set of medium crates once
		3.	Capture:
			•	Instrumentor crashes
			•	Runtime violations
			•	First violation signature per run
			•	Canonicalize violation signatures (kind + access type + pointer kind + size) to avoid nondeterministic address noise
		4.	Define two execution profiles:
			•	FAST: minimal logging, stop at first violation
			•	DEBUG: RZ_LOG=Trace, preserve MIR dumps for first failing case only

Exit Criteria
	•	Running the harness twice yields the same outcome (pass or same first violation)
	•	No nondeterministic crashes or random epoch mismatches

Status
	•	Completed: scripts/run_harness.sh exists with FAST/DEBUG profiles and capture logic.

⸻

Phase 1 — Call Boundary Semantics + Std/Core Classification (COMPLETED)

Goal
	•	Reduce false positives at call boundaries; remove “unknown call” noise when deps are instrumented.

Implementation
	•	Argument passing:
		•	record coarse ESCAPE/USE for pointer arguments (conservative)
		•	do not treat argument passing itself as READ/WRITE
	•	Pointer returns:
		•	recover tags across instrumented calls via ret-tag side-channel
		•	fallback when missing: synthesize a root raw tag (avoid UNKNOWN_TAG)
	•	Unknown calls:
		•	conservative policy (read/write/escape) when truly unclassified and uninstrumented
	•	Std/core call classification:
		•	extend matcher to cover def-path normalization cases
	•	Static memory:
		•	image/segment scan so vtable/.rodata reads are not flagged as WILD_POINTER

Status
	•	Bytes/smallvec build+run stable in debug+release.
	•	UNKNOWN_TAG in medium_smallvec_driver fixed via return-tag fallback (__rz_take_ret_tag_or_root).

⸻

Phase 2 — Allocation Model Completeness (Epoch Soundness) (COMPLETED)

Goal
	•	Make allocation tracking correct enough that safe code does not trip stale-epoch bugs.

Implementation
	•	Intercept and record:
		•	allocation
		•	deallocation
		•	reallocation (critical for Vec)
	•	On realloc:
		•	if realloc returns same base: keep epoch, update size
		•	if realloc moves: bump epoch on old base, mark old dead, new base gets fresh epoch

Status
	•	Same-base realloc preserves epoch; moved realloc marks old dead and records new base.
	•	Micro example added: examples/realloc_same_base (expected ok).

⸻

Phase 3 — Stacked Borrows (Lite Version) (COMPLETED)

Goal
	•	Add aliasing bugs as a first-class bug class with minimal complexity.

Implementation (Lite)
	•	Retag points:
		•	reference creation
		•	function entry for reference arguments
	•	Per-allocation borrow stack:
		•	&mut adds a unique entry
		•	& adds a shared entry
		•	unique reborrow invalidates/truncates newer stack state (SB-style)
	•	Access checks:
		•	READ allowed if compatible
		•	WRITE requires top unique
	•	UnsafeCell carve-out / opt-out marker to avoid exploding on interior mutability patterns.

Status
	•	SB-lite implemented with per-allocation stack + invalidation on unique reborrow.
	•	Micro suite added; scripts/run_example_tests.py passes.

⸻

Phase 4 — Tree Borrows (Precision Upgrade) (NEXT)

Goal
	•	Reduce SB-lite false positives while keeping strong bug-finding power.

Implementation
	•	Replace stack with derivation tree:
		•	parent pointer
		•	permission/state bits
	•	On conflict:
		•	invalidate subtree
		•	enforce parent-child permission rules on reads/writes
	•	Maintain a clean “mode switch”:
		•	coverage mode (low-FP, exploration)
		•	precision mode (SB-lite/TB enabled)

Testing Gate
	•	Re-run all micro-suite + bytes/smallvec/serde in both modes
	•	Compare violation count and stability vs SB-lite

Exit Criteria
	•	TB reduces false positives vs SB-lite on medium crates
	•	Micro violations remain caught and stable

⸻

Phase 5 — Bigger Crates & Ecosystem Coverage

Goal
	•	Scale to real software without instrumentor crashes and without “noise storms”.

Suggested targets
	•	ripgrep
	•	reqwest / hyper
	•	rust-analyzer components

Strategy
	•	Run in coverage mode first (conservative unknown calls, minimal alias enforcement)
	•	Then re-run in precision mode (SB-lite/TB enabled)

⸻

Phase 6 — Fuzzing + Evaluation (Paper Track)

Goal
	•	Make results defensible for a first-tier security paper and practical for fuzzing workflows.

What “paper-ready” requires
	•	Clear claim/scope: dynamic MIR instrumentation + runtime checks for memory-safety bugs.
	•	Stable toolchain story: reproducible outputs, low false positives on safe code, triage/dedup pipeline.
	•	Real bugs: previously-unknown issues in widely-used crates (or strong results on known UB corpora + comparisons).
	•	Quantitative evaluation: precision, coverage, overhead, stability.

Implementation
	•	Fuzzing harness:
		•	AFL++ harness (current): `afl_harness` crate with `afl_bytes_driver` / `afl_smallvec_driver`
			•	build: `scripts/afl_build.sh`
			•	fuzz: `scripts/afl_fuzz.sh`
			•	repro: `scripts/afl_repro.sh`
			•	Docker (Linux): `docker/afl/README.md` + `scripts/docker_afl.sh`
		•	Optional: cargo-fuzz/libFuzzer later for tighter in-process integration
		•	deduplicate by canonical signature (kind + access + ptr kind + size + callsite)
		•	store repro inputs (+ optional minimization)
	•	Triage pipeline:
		•	group violations by signature
		•	auto-attach: backtrace/location, crate/version, mode (coverage vs precision), seed
	•	Metrics collection:
		•	runtime slowdown vs baseline (+ ASan)
			•	`python3 scripts/bench_overhead.py --include-asan ...`
		•	peak memory overhead
		•	instrumentation/build time overhead
		•	stability across runs (same input => same signature)

Benchmarks
	•	Micro UB suite: existing examples + curated UB corpus
	•	Medium “safe” set: bytes, smallvec, serde/serde_json (instrumented tests or drivers)
	•	Large set: ripgrep or reqwest/hyper
	•	Comparisons (as feasible): Miri, ASan/UBSan, at least one dynamic Rust tool

Exit Criteria
	•	Fuzz targets run without ICEs/crashes in the tool
	•	Violations are reproducible and explainable (deduped signatures)
	•	Non-trivial set of actionable issues with minimized repros
	•	Overhead numbers reported for coverage vs precision modes

Immediate Next Actions (Concrete)
	1.	Fuzzing hygiene:
		•	ensure `fuzz/out/` is never committed (gitignore + `git rm --cached` if needed)
		•	build fuzz targets with `-C panic=abort` to avoid unwind cascades masking the first violation
	2.	Crash triage loop (bytes first):
		•	repro each crash with `scripts/afl_repro.sh`
		•	minimize with `afl-tmin` and keep a `fuzz/repro/<target>/` folder with minimized repros
		•	classify: real UB in target crate vs false positive (e.g., `alias_exempt` / missing modeling)
		•	turn stable crashes into:
			•	a new `examples/*` regression, or
			•	a pinned repro input + a short write-up under `reports/`
	3.	Add initial fuzz targets:
		•	smallvec (same pipeline as bytes)
		•	serde_json (driver/harness once third_party is available)
	4.	Evaluation (start lightweight, then scale):
		•	run overhead on examples + medium (`scripts/bench_overhead.py`), baseline vs rusteze vs ASan
		•	track “violations per hour” and “unique signatures” for each fuzz target/mode
	5.	Define TB design and implement Phase 4 behind a flag once fuzzing produces stable signal.
