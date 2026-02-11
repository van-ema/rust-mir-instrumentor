Roadmap

Current status
	•	Phase 0 (harness): completed
	•	Phase 1 (call boundaries + std/core classification): completed (bytes/smallvec debug+release stable)
	•	Phase 2 (alloc/realloc epochs): completed
	•	Phase 3 (SB-lite): completed (micro-suite + example tests passing)
	•	Phase 4 (Tree Borrows, runtime-first): in progress (`tb_lite` selectable, dedicated micro-suite added, call-argument protectors + 2-phase Reserved(conflicted)/activation transitions implemented)
	•	Phase 6 (Fuzzing + Evaluation): in progress (AFL++ harness + Docker workflow; bytes/smallvec/serde_json smoke loops stable)
Next step
	•	Complete Phase 4 by expanding protector/state-machine coverage (per-location transitions, weak/strong protector nuances, protector-end access semantics)
	•	In parallel: continue Phase 6 to turn fuzz findings into minimized, reproducible, triaged reports
	•	Phase 7 (Wide/Fat pointers) to support slices/str/dyn Trait and reduce blind spots in real crates

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

Work packages
	1.	Model scaffold (runtime)
		•	add `runtime/src/alias_model/tree_borrows_lite.rs`
		•	register in `runtime/src/alias_model/mod.rs` and env selector
		•	add optional tracing envs (`RZ_TB_LITE`, `RZ_TB_DUMP`) mirroring SB-lite style
	2.	Tree state representation
		•	per-allocation forest keyed by tag/root
		•	node metadata:
			•	parent
			•	range (`pointee_addr`, `bounds_len`)
			•	permission/state (initially minimal, then refined)
			•	alive/invalidated marker
	3. Retag + creation rules
		•	on `RefMut` creation: enforce uniqueness/child transition rules
		•	on `RefShared` creation: create shared child without eager global invalidation
		•	raw creation keeps lineage and participates in access checks through nearest ref ancestor
	4. Access rules (tb_lite semantics)
		•	READ: check current node and relevant ancestors/active blockers
		•	WRITE: require write-capable path; invalidate conflicting branches
		•	preserve `UnsafeCell` / alias-exempt carve-outs
	5. Call-boundary protectors (implemented in current lite form)
		•	record consumed call-arg parent tags (`__rz_take_call_arg_tag`)
		•	mark immediate child ref tags as protected for callee lifetime
		•	enforce:
			•	write via overlapping non-protected tag => `TB_LITE_PROTECTOR_CONFLICT`
			•	heap dealloc while protected tag still active => `TB_LITE_PROTECTOR_DEALLOC`
		•	pop protector frame at callee return via `__rz_exit_fn` hook
	6. Lifetime integration
		•	drop per-allocation tree state on alloc death (`on_alloc_state_change`)
		•	avoid stale-tree conflicts across epoch reuse
	7. Diagnostics
		•	emit TB-specific violation reasons with parent/root/range context
		•	keep violation kind stable (`STACKED_BORROWS_VIOLATION`) initially, then split if needed
	8. Model-specific test expectations
		•	`scripts/run_example_tests.py` supports model-specific expectation files:
			•	`expected.<bin>.<model>.rz`
			•	`expected.<model>.rz`
		•	added `examples/tb_miri_micro` with Miri-inspired tests and per-model expectations

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
		•	Update: proceed runtime-first now (`RZ_ALIAS_MODEL=tb_lite`), then add instrumentation
		  events only for demonstrated missing-signal cases.

⸻

Phase 7 — Wide/Fat Pointer Support (Slices/str/dyn Trait) (NEXT)

Goal
	•	Handle Rust wide pointers so real-world crates using `&[T]`, `&str`, and `dyn Trait` are not
	  silently untracked or incorrectly tagged.

Current limitation
	•	The instrumentation/runtime primarily assumes thin pointers. Wide pointers carry metadata
	  (slice length, vtable) and require extracting the *data pointer* for allocation lookup and tagging.
	•	We often fall back to `size=0` for unsized pointees, weakening OOB checks and lineage propagation.

Implementation plan (incremental)
	1.	Data-pointer extraction:
		•	for wide pointer locals used in deref/read/write hooks, extract the data pointer and pass it to runtime hooks
		  (`expose_provenance` on the data pointer, not on the wide pointer container).
	2.	Tag association:
		•	associate tags with the *data pointer address* (thin `*const ()` / `*mut ()`) even when the source is wide.
		•	ensure derivations (casts, unsize coercions, from_raw_parts) preserve lineage of the data pointer.
	3.	Best-effort access sizing:
		•	slices: size = `len * size_of::<T>()` when available
		•	str: size = `len`
		•	dyn Trait: size unknown (keep size=0 but still track address/epoch)
	4.	Tests:
		•	add micro-examples for raw reads/writes through `&[u8]` and `&str`
		•	add at least one example for vtable/dyn-trait data-pointer tracking

Exit criteria
	•	No UNKNOWN_TAG / WILD_POINTER for ordinary slice/str operations in bytes/smallvec drivers.
	•	Micro-suite covers wide pointer cases and remains stable.

Status (as of 2026-02-04)
	•	(1) + (2) implemented for the most common paths:
		•	address extraction for hooks uses a thin data pointer for wide pointers
		•	wide pointer locals are treated as tag relevant so lineage isn’t dropped before data-pointer extraction
		•	best-effort same-block unsize backtracking preserves the source local for `&[T]` produced via coercion
	•	(3) partially implemented:
		•	slice/str metadata length is recorded in tags and checked against accesses when available
		•	index-based OOB can still slip through when MIR does not expose the element offset
	•	(4) partially implemented:
		•	new wide-pointer micro-examples exist and are stable under `scripts/run_example_tests.py`
	•	Remaining:
		•	metadata-aware sizing for memcpy/memmove-like intrinsics on wide pointers
		•	dyn-trait size remains unknown (size=0)
