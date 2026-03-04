Roadmap

Current status
	•	Phase 4 (Tree Borrows, runtime-first): in progress (`tb_lite` selectable, dedicated micro-suite added, call-argument protectors + 2-phase Reserved(conflicted)/activation transitions implemented)
	•	Phase 5 (bigger crates / ecosystem coverage): pending expansion beyond the current target set
	•	Phase 6 (Fuzzing + Evaluation): in progress (AFL++ harness + Docker workflow; bytes/smallvec/serde_json smoke loops stable)
	•	Phase 7 (Wide/Fat pointers): remaining work is focused on bulk-memory ops, dyn-trait coverage, and wider validation
Next step
	•	Complete Phase 4 by expanding protector/state-machine coverage (per-location transitions, weak/strong protector nuances, protector-end access semantics)
	•	In parallel: continue Phase 6 to turn fuzz findings into minimized, reproducible, triaged reports
	•	Complete the remaining Phase 7 work to reduce wide-pointer blind spots in real crates

⸻

Phase 4 — Tree Borrows (Precision Upgrade, Runtime-First) (IN PROGRESS)

Goal
	•	Introduce a Tree-Borrows-style alias model with better precision than SB-lite while
	  preserving existing bug-finding power and fuzzing stability.

Approach
	•	Runtime-first implementation in the existing pluggable alias-model layer:
		•	add `tb_lite` as a new `AliasModel`
		•	select with `RZ_ALIAS_MODEL=tb_lite` (now the default; `sb_lite` remains opt-in)
	•	Instrumentation stays unchanged in the first iteration.
	•	If needed, add targeted instrumentation events only after measuring concrete TB gaps.

Remaining work packages
	1.	Tree state representation
		•	per-allocation forest keyed by tag/root
		•	node metadata:
			•	parent
			•	range (`pointee_addr`, `bounds_len`)
			•	permission/state (initially minimal, then refined)
			•	alive/invalidated marker
	2. Retag + creation rules
		•	on `RefMut` creation: enforce uniqueness/child transition rules
		•	on `RefShared` creation: create shared child without eager global invalidation
		•	raw creation keeps lineage and participates in access checks through nearest ref ancestor
	3. Access rules (tb_lite semantics)
		•	READ: check current node and relevant ancestors/active blockers
		•	WRITE: require write-capable path; invalidate conflicting branches
		•	preserve `UnsafeCell` / alias-exempt carve-outs
	4. Call-boundary protector refinement
		•	enforce:
			•	write via overlapping non-protected tag => `TB_LITE_PROTECTOR_CONFLICT`
			•	heap dealloc while protected tag still active => `TB_LITE_PROTECTOR_DEALLOC`
		•	model weak/strong protector nuances and protector-end semantics more precisely
	5. Lifetime integration
		•	drop per-allocation tree state on alloc death (`on_alloc_state_change`)
		•	avoid stale-tree conflicts across epoch reuse
	6. Diagnostics
		•	emit TB-specific violation reasons with parent/root/range context
		•	keep violation kind stable (`STACKED_BORROWS_VIOLATION`) initially, then split if needed

Validation gates
	•	Gate A: examples
		•	run `scripts/run_example_tests.py` in both `sb_lite` and `tb_lite`
		•	no regressions in known must-catch UB examples
	•	Gate B: medium crates
		•	run bytes/smallvec debug+release in both models
		•	verify lower or equal false-positive count in `tb_lite`
	•	Gate C: fuzzing sanity
		•	run short AFL++ sessions on bytes/smallvec/serde_json
		•	ensure no runtime panics/ICE-equivalent behavior introduced by `tb_lite`

Expected follow-up (if runtime-only is insufficient)
	•	Add minimal instrumentation support for TB-specific events:
		•	explicit invalidation points for fresh unique borrows
		•	protector/lifetime hints for call boundaries and returns
	•	Only add these after a reproducible failing case demonstrates missing signal.

Exit criteria
	•	`tb_lite` selectable and stable on examples + medium crates.
	•	At least one previously noisy SB-lite pattern is clean in `tb_lite`.
	•	No loss of detection on core UB examples (UAF/OOB/alias conflicts).

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

Open work
	1.	Crash triage loop
		•	repro each crash with `scripts/afl_repro.sh`
		•	minimize with `afl-tmin` and keep a `fuzz/repro/<target>/` folder with minimized repros
		•	classify: real UB in target crate vs false positive (e.g., missing modeling, bad lineage, over-broad heuristics)
		•	turn stable crashes into:
			•	a new `examples/*` regression, or
			•	a pinned repro input + a short write-up under `reports/`
	2.	Target expansion
		•	add and stabilize more high-value fuzz targets beyond the current baseline set
	3.	Evaluation
		•	run overhead on examples + medium (`scripts/bench_overhead.py`), baseline vs rusteze vs ASan
		•	track “violations per hour” and “unique signatures” for each fuzz target/mode

⸻

Phase 7 — Wide/Fat Pointer Support (Slices/str/dyn Trait) (NEXT)

Goal
	•	Handle Rust wide pointers so real-world crates using `&[T]`, `&str`, and `dyn Trait` are not
	  silently untracked or incorrectly tagged.

Current limitation
	•	The instrumentation/runtime primarily assumes thin pointers. Wide pointers carry metadata
	  (slice length, vtable) and require extracting the *data pointer* for allocation lookup and tagging.
	•	We often fall back to `size=0` for unsized pointees, weakening OOB checks and lineage propagation.

Remaining work
	1.	Best-effort access sizing:
		•	slices: size = `len * size_of::<T>()` when available
		•	str: size = `len`
		•	dyn Trait: size unknown today; improve this where possible
	2.	Memory-copy and bulk-op support:
		•	metadata-aware sizing for memcpy/memmove-like intrinsics on wide pointers
	3.	Tests and validation:
		•	add micro-examples for raw reads/writes through `&[u8]` and `&str`
		•	add at least one example for vtable/dyn-trait data-pointer tracking

Exit criteria
	•	No UNKNOWN_TAG / WILD_POINTER for ordinary slice/str operations in bytes/smallvec drivers.
	•	Micro-suite covers wide pointer cases and remains stable.
