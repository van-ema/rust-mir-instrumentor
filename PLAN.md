Phase 0 — Lock in a Repeatable Test Harness (COMPLETED)

Goal

Make failures reproducible, classifiable, and stable before adding new semantics.

Tasks
	•	Create a script: scripts/run_harness.sh
	•	The script should:
	1.	Run flaky micro-examples (e.g. memset_u8_dynamic) 200–1000 times
	2.	Run a small set of medium crates once
	3.	Capture:
	•	Instrumentor crashes
	•	Runtime violations
	•	First violation signature per run
	•	Canonicalize violation signatures (e.g., kind + access type + pointer kind + size) to avoid nondeterministic address noise
	•	Define two execution profiles:
	•	FAST
	•	Minimal logging
	•	Stop at first violation
	•	DEBUG
	•	RZ_LOG=Trace
	•	Preserve MIR dumps for first failing case only

Exit Criteria
	•	Running the harness twice yields:
	•	the same outcome (pass or same first violation)
	•	no nondeterministic crashes or random epoch mismatches

Status
	•	Completed: `scripts/run_harness.sh` exists with FAST/DEBUG profiles and capture logic.

⸻

Phase 1 — Call Boundary Semantics (High-Leverage, Low-Cost)

This phase dramatically reduces false positives before SB/TB.

Implementation

1. Argument Passing (Minimum Viable Semantics)
	•	When a pointer/reference is passed as a function argument:
	•	Record ESCAPE(tag) (conservative)
	•	Do not treat argument passing as READ/WRITE.
	•	Returned pointers:
	•	Create a fresh tag
	•	Parent = UNKNOWN (tag=0), snapshot alloc_epoch from pointee allocation

Status
	•	Argument passing escape events are emitted at call boundaries.
	•	Unknown call policy implemented (read/write/escape for pointer args).
	•	Common helpers classified (Deref, Iterator adaptors, slice iter).

2. Unknown Call Policy
Adopt a single consistent policy:

Unknown calls may read/write/escape all pointer arguments.

Concretely:
	•	For every pointer argument:
	•	mark as escaped
	•	Optionally log a warning (once per function)

3. Small Call Classification Table
Manually whitelist a few common patterns:
	•	Deref::deref
	•	iterator adaptors (next, nth)
	•	slice helpers
	•	Vec::as_ptr / Vec::as_mut_ptr
	•	slice::as_ptr / slice::as_mut_ptr
	•	ptr::copy*, memcpy, memset

Even 5–10 entries reduce noise massively.

Testing Gate

Micro
	•	Existing memcpy/memset examples
	•	Run 1000× without nondeterminism

Medium Crates (START NOW)
	•	bytes
	•	smallvec
	•	serde (compile + subset of tests)

Exit Criteria
	•	No random violations across runs
	•	Fewer “unknown call with pointer effects” warnings
	•	Violations are explainable and stable

⸻

Phase 2 — Allocation Model Completeness (Epoch Soundness)

Before aliasing checks, allocation tracking must be correct.

Implementation
	•	Intercept and record:
	•	allocation
	•	deallocation
	•	reallocation (critical for Vec)
	•	On realloc:
	•	if realloc returns same base: keep epoch, update size
	•	if realloc moves: bump epoch on old base, mark old dead, new base gets fresh epoch

Store per allocation:
	•	base
	•	size
	•	epoch
	•	live/dead flag

Testing Gate

Micro
	•	Stress tests:
	•	Vec::push/pop/reserve/extend
	•	String growth and shrink

Medium
	•	bytes
	•	regex

Exit Criteria
	•	No stale-epoch violations from pure safe code
	•	Realloc-heavy workloads are stable

⸻

Phase 3 — Stacked Borrows (Lite Version)

This introduces real aliasing checks with minimal complexity.

Implementation (Lite)
	•	Add retag points:
	•	reference creation
	•	function entry for reference arguments
	•	Maintain a per-allocation stack:
	•	&mut adds a unique entry
	•	& adds a shared entry
	•	On access:
	•	READ allowed if compatible
	•	WRITE requires top unique
	•	Otherwise, report a violation

Add an explicit UnsafeCell carve-out before Medium crates (skip alias checks for allocations containing UnsafeCell or an opt-out marker).

Testing Gate

Micro
	•	Two &mut to same location
	•	& + write through raw pointer
	•	Reborrow chains

Medium
	•	smallvec
	•	bytes
	•	clap

Exit Criteria
	•	Expected micro violations are caught
	•	Medium crates do not explode with false positives

⸻

Phase 4 — Tree Borrows (Precision Upgrade)

Reduce false positives caused by stacked discipline.

Implementation
	•	Replace stack with derivation tree:
	•	parent pointer
	•	permission bits
	•	On conflict:
	•	invalidate subtree
	•	Enforce parent-child permission rules

Testing Gate
	•	Re-run all previous medium crates
	•	Compare violation count and stability vs SB-lite

⸻

Phase 5 — Bigger Crates & Ecosystem Coverage

When to Start Big Crates

You are ready when:
	•	Medium crates run without instrumentor crashes
	•	Violations are reproducible and classifiable

Suggested Big Crates
	•	ripgrep
	•	reqwest / hyper
	•	rust-analyzer components

Run in:
	•	coverage mode first (conservative unknown calls)
	•	precision mode later (SB/TB enabled)

⸻

Phase 6 — Evaluation (Make Results Defensible)

Metrics
	•	Runtime overhead (median + p95 slowdown)
	•	Memory overhead
	•	Instrumentation time
	•	Violation count and stability across runs

Baselines
	•	Uninstrumented build
	•	Coverage-only instrumentation (no checks)
	•	SB-lite and TB modes

Datasets
	•	Micro-suite (existing examples)
	•	Medium crates: bytes, smallvec, serde
	•	Large crate: ripgrep or hyper/reqwest
	•	Curated UB corpus (known UAF/OOB repros)

Exit Criteria
	•	Overhead numbers are reported for each profile
	•	Violations are reproducible and explainable

⸻

Definition of “Done” (Practical)

You are done when:
	•	Fuzz targets run without ICEs or crashes
	•	You reliably catch:
	•	OOB
	•	UAF
	•	double free
	•	write-through-shared-ref
	•	cross-call aliasing bugs
	•	Unknown-wrapper warnings are rare or non-fatal
	•	Results are stable across runs

⸻

Immediate Next Actions (Concrete)
	1.	Implement Phase 1 call-boundary semantics
	2.	Start running bytes + smallvec now
	3.	Only then move to SB-lite

⸻
