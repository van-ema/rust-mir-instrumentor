# Interprocedural Whole-Program Dataflow Design

## Goal

Add a conservative interprocedural analysis that improves MIR instrumentation decisions without
changing program behavior or introducing false positives.

The primary use cases are:
- better cross-crate call/return modeling
- safer pruning of metadata-propagation hooks
- whole-program unsafe/pointer-relevance propagation

The analysis must follow the project soundness rule:
- every analysis-driven optimization must be sound by default
- if soundness is not established yet, it stays opt-in

This design is intentionally **summary-based**. It is not a single-pass "load the whole world into
`instrumentation.rs`" approach.

## Why a two-pass design is needed

`instrument-mir/src/instrumentation.rs` runs with a current-crate MIR view. That is enough for local
instrumentation, but not for a true whole-program dataflow.

Problems we cannot solve with a single local pass:
- cross-crate call graph propagation
- callee effects in dependencies
- propagated unsafe/pointer relevance through helper layers
- better return-provenance modeling across crate boundaries

The practical design is therefore:
1. compile once to collect per-crate summaries
2. merge them into a whole-program index
3. compile again using that index to guide instrumentation

This is closer to ThinLTO summary collection than to a traditional local MIR pass.

## Non-goals

This design is not trying to provide:
- an exact whole-program call graph
- a verifier
- a proof of Rust alias semantics
- a license to rewrite semantic hooks (`PtrRead`, `PtrWrite`, `PtrUse`) based only on abstract
  pointer equivalence

The current provenance-dataflow experiment showed why that last point is unsafe: pointer-provenance
equivalence is not enough to prove that the tag/ref-ancestor locals consumed by instrumentation are
synchronized.

## High-level pipeline

### Pass A: summary collection build

Each crate is compiled once in an `analyze-only` mode.

Output:
- one summary file per crate
- no interprocedural optimization decisions yet

Suggested location:
- `target/rusteze-summaries/<crate-key>.json`

### Merge step

After the first build finishes, merge all crate summaries into a single whole-program index.

Output:
- `target/rusteze-index/program_index.json`

This index contains:
- conservative cross-crate call edges
- propagated unsafe/pointer relevance
- per-function input/output pointer effects
- summary information for call/return instrumentation decisions

### Pass B: guided instrumentation build

Rebuild the program from scratch.

During this second build, `instrument-mir` loads the merged index and uses it to:
- refine call/return handling
- decide where metadata propagation is required
- prune only propagation hooks proven redundant under the summary model

## Summary schema

Start simple. The first version should export function-level summaries, not full MIR state.

Suggested schema per function:

- `function_id`
  - stable path-like key, for example crate key + def-path hash
- `unsafe_sensitive`
  - whether the function directly performs unsafe-relevant pointer operations
- `calls_unknown`
  - whether the function calls an unmodeled callee
- `ptr_arg_count`
- `ptr_ret_kind`
  - `none`
  - `fresh`
  - `from_arg(i)`
  - `unknown`
- `arg_effects[i]`
  - `read_deref`
  - `write_deref`
  - `create_ref`
  - `create_raw`
  - `escape`
  - `forward_to_ret`
  - `forward_to_call`
- `metadata_relevant`
  - whether local metadata propagation in this function matters to callers/callees
- `callees`
  - conservative list of known direct callees

This is enough to make the second build smarter without overcommitting to a brittle summary format.

## Soundness rules

The summaries must be conservative.

Required properties:
- missing call edge is bad
- extra call edge is acceptable
- missing pointer effect is bad
- extra pointer effect is acceptable
- unknown behavior must degrade to "instrument more", not "instrument less"

If the merged index is missing, stale, or inconsistent:
- fall back to existing local instrumentation behavior
- do not partially apply speculative pruning

## What the analysis should optimize first

The first use of the index should be narrow and safe.

### Good first targets

1. Call/return modeling
- improve `CallArgPush` / `ArgRetag` / `RetPush` / return-derive decisions
- use callee summaries to decide whether return provenance can come from an argument

2. Metadata-propagation pruning
- prune only propagation hooks such as:
  - `TagProp`
  - ref-ancestor propagation
  - dead metadata stores

3. Unsafe/pointer relevance propagation
- mark helper functions as metadata-relevant even if they are locally "boring"
- keep instrumentation where a local-only analysis would underapproximate

### Things to avoid initially

Do not use the whole-program index to rewrite or suppress:
- `PtrRead`
- `PtrWrite`
- `PtrUse`
- other semantic observation points

Those hooks are where the runtime actually checks behavior. They should continue to observe the
original local's metadata unless we later prove equivalence at the metadata-local level.

## Unsafe-sensitive reachability plan

The pruning criterion must not be ``textually inside an `unsafe {}` block''.
That boundary is too weak: safe-looking code may call dependencies or standard-library APIs that
execute unsafe internals, and the final semantic hook may occur outside the block that introduced
the problematic provenance.

The right property is:

- a pointer/reference and its aliases may be pruned only if they can be proven never to reach any
  **unsafe-sensitive sink**

Initial sink set:
- raw-pointer creation/derivation
- raw dereference
- ref-from-raw / raw-from-ref sensitive paths
- pointer escape to unknown or uninstrumented calls
- FFI/intrinsics
- call/return boundaries whose summaries are unknown

This definition is conservative by construction: if we are unsure whether a sink is
unsafe-sensitive, we classify it as relevant and keep instrumentation.

## Staged implementation plan

The implementation should proceed in small, validated steps.

### Step 1: tighten the existing intra-procedural analysis

Extend `instrument-mir/src/unsafe_dataflow.rs` so it computes a stronger local notion of
unsafe-sensitive influence:

- identify unsafe-sensitive roots
- propagate through pointer copies, reborrows, and pointer casts
- model escapes to calls and returns conservatively
- treat unknown calls as sinks, not as pruning opportunities

This stage remains purely intra-procedural and should only affect hook placement, not hook
retargeting.

### Step 2: add per-function summaries

For each function, compute a compact summary such as:

- whether it is directly unsafe-sensitive
- which pointer arguments reach unsafe-sensitive sinks
- which arguments escape
- whether the return value is fresh, derived from an argument, or unknown
- whether the function calls unknown or already-unsafe-sensitive callees

The first version can stay crate-local and need not serialize summaries yet.

### Step 3: add intra-crate interprocedural propagation

Build a conservative crate-local call graph and propagate the summaries:

- if callee argument `i` is unsafe-sensitive, the corresponding caller argument becomes relevant
- if the callee return derives from argument `i`, propagate relevance back to the caller
- if the callee is unknown or incomplete, fall back to keeping instrumentation

This stage should already strengthen pruning over helper-heavy code without requiring a two-pass
whole-program build.

### Step 4: use summaries only for pruning, not semantic-hook rewrites

Initially, summary results should only drive:

- pruning of `PtrRead` / `PtrWrite` on values proven outside the unsafe-sensitive slice
- maybe later, pruning of obviously irrelevant `Ref` / `Raw` creation hooks

They must not be used to:

- retarget semantic hooks to different locals
- assume pointer-equivalence implies tag-local equivalence

### Step 5: move to cross-crate/two-pass summaries

Once the intra-crate summary model is stable and validated:

1. emit per-crate summaries in an analyze-only build
2. merge them into a whole-program index
3. rerun compilation with the merged index guiding instrumentation

Only at this point should cross-crate pruning become aggressive.

## Validation order

Each stage must clear the same gates before proceeding:

1. full example suite stays green
2. representative AFL smoke targets build and start fuzzing without immediate tool aborts
3. differential comparison against the full-instrumentation mode shows no regressions on known UB
   examples

This ensures every pruning step is justified by evidence rather than by intuition.

## Progress
## Crate-Local Fixed-Point Algorithm

The reverted summary-consumer path used **intra-procedural** per-function summaries as if they were already interprocedural. That was the wrong abstraction boundary. The result was not a clean pruning win: a few `escape_unknown` bits went down, but many more caller functions became classified as `has_direct_sink` or `calls_unknown_boundary`, so the overall unsafe-sensitive slice grew.

The correct next algorithm is a **crate-local fixed point** over local function summaries.

### Summary of the failure mode

Producer summaries currently mean:
- what happens inside function `F`'s body itself
- with modeled external callees treated via built-in conservative summaries
- without propagating effects through local callees to a fixed point

Consumer mode tried to use those summaries at call sites anyway:
- if callee arg `i` reached a direct sink, mark caller actual arg `i` unsafe-relevant
- if callee arg `i` escaped to unknown, mark caller actual arg `i` unsafe-relevant
- if callee arg `i` forwarded to return, taint the caller destination

That is only valid once the callee summaries themselves already include local-callee effects.

### Phase A: build base summaries for all local functions

For every local MIR body `F`, compute a summary **without consulting local callees**.

Conceptually:

```rust
struct FunctionBase {
    def_id: LocalDefId,
    ptr_arg_count: usize,
    local_summary: UnsafeFunctionSummary,
    callsites: Vec<CallSiteInfo>,
}

struct CallSiteInfo {
    callee: Option<DefId>,
    actual_ptr_args: Vec<(usize, Local)>,
    dest_local: Option<Local>,
}
```

`local_summary` should only contain effects from `F` itself:
- raw deref
- raw creation / provenance-sensitive casts
- direct raw-sensitive call use
- unknown external boundaries
- direct return forwarding visible in `F`

Local callees are recorded in `callsites`, not recursively expanded.

### Phase B: build the crate-local call graph

From `callsites`, build edges `F -> G` for local callees only.

Then run a worklist or SCC-based fixed point.

### Phase C: propagated summary state

Use the same summary shape already in the codebase:

```rust
struct PropagatedSummary {
    has_direct_sink: bool,
    calls_unknown_boundary: bool,
    ptr_args: Vec<UnsafeArgSummary>,
}
```

Per pointer arg, keep:
- `direct_sink_mask`
- `propagation_mask`

Initialize:

```text
prop_summary[F] = base.local_summary[F]
```

### Phase D: fixed-point propagation

Iterate until no summary changes. For each caller `F`:

1. Start from `base.local_summary(F)`.
2. For each local callsite `F -> G`:
   - read current `prop_summary[G]`
   - for each mapped pointer arg `(arg_index, caller_local)`:
     - if `G.ptr_args[arg_index].direct_sink_mask != 0`, seed `caller_local` in `F` as sink-reaching
     - if `G.ptr_args[arg_index].propagation_mask` contains `ESCAPE_UNKNOWN`, seed `caller_local` as escaping unknown
     - if `G.ptr_args[arg_index].propagation_mask` contains `FORWARD_TO_RETURN`, seed the call destination as derived from `caller_local`
3. Re-run `F`'s intra-procedural transfer using those call-derived seeds.
4. Compare the new summary with `prop_summary[F]`.
5. If it changed, enqueue callers of `F`.

Important: the call-derived facts are **inputs to `F`'s local analysis**, not ad hoc post-processing on the final summary.

### Example: sink propagation through wrappers

```rust
fn h(p: *const u8) {
    unsafe { *p; }
}

fn g(p: *const u8) {
    h(p);
}

fn f(p: *const u8) {
    g(p);
}
```

Base summaries:
- `h(arg0)` has `direct_sink`
- `g(arg0)` does not yet
- `f(arg0)` does not yet

After fixed point:
- `h(arg0)` direct sink
- `g(arg0)` direct sink
- `f(arg0)` direct sink

### Example: return forwarding through helpers

```rust
fn id_ref<'a>(p: &'a [u8]) -> &'a [u8] { p }

fn wrap<'a>(p: &'a [u8]) -> &'a [u8] {
    id_ref(p)
}
```

Base summaries:
- `id_ref(arg0)` has `FORWARD_TO_RETURN`
- `wrap(arg0)` may not yet

After fixed point:
- `wrap(arg0)` also has `FORWARD_TO_RETURN`

### Soundness constraints

- missing callee summary => conservative fallback
- unknown external callees remain unknown
- summary bits only grow during propagation
- callsite arg mapping must be exact for pointer args
- do not consume summaries for pruning until they are fixed-point summaries

### Implementation boundary

This should be implemented **outside** the current on-demand `optimized_mir` override path.

Do not:
- query local callee MIR on demand while instrumenting another function
- use raw intra-procedural summaries directly for pruning

Do:
- collect all local base summaries first
- propagate them crate-wide to a fixed point
- only then use them as analysis results


### Step 1 implemented: stronger intra-procedural unsafe-sensitive analysis

The first step of the plan is now implemented in
`instrument-mir/src/unsafe_dataflow.rs`.

Current improvements:

- track **tainted value carriers** in addition to tainted pointer locals
  - this preserves unsafe-sensitive influence when a pointer flows through a local wrapper or
    aggregate before being extracted again
- treat **provenance-sensitive pointer roots** as unsafe-sensitive roots
  - `Rvalue::RawPtr(..)`
  - `CastKind::PointerWithExposedProvenance` to a pointer type
  - pointer-producing `Transmute` from integral sources, conservatively
- propagate taint through **aggregate/projection storage**
  - storing a tainted pointer into `base.field` taints the base local as a carrier
- keep **unknown or uninstrumented call boundaries** as sinks
  - destinations of such calls become tainted when fed by tainted inputs or raw arguments

This remains a hook-placement analysis only. It does not retarget semantic hooks and does not
rewrite tag/ref-ancestor consumers.

### Practical examples

Example: wrapper local keeps unsafe-sensitive influence

```rust
let p: *mut u8 = ...;
let w = Wrapper { p };
let q = w.p;
unsafe { *q = 1; }
```

The old analysis could lose the connection at `w`. The current version taints `w` as a value
carrier, so `q` remains unsafe-relevant.

Example: provenance-sensitive cast is treated as a root

```rust
let addr: usize = ...;
let p = addr as *const u8;
unsafe { *p };
```

This is now conservatively treated as unsafe-sensitive from the cast onward.

Example: projected storage preserves relevance

```rust
holder.ptr = raw_ptr;
let q = holder.ptr;
unsafe { *q };
```

The base local `holder` is tainted as a carrier, so extracting `q` keeps the unsafe-sensitive
slice intact.

### Validation status

The current Step 1 implementation has been validated with:

- `cargo build -p instrument-mir`
- `cargo build -p runtime`
- full example suite:
  - `reports/example_tests/20260311_120700/summary.tsv`

Further work still required:

- propagate those summaries across the crate-local call graph
- measure pruning impact with better aggregated stats than the current per-build log sampling

### Step 2 implemented: per-function summaries and dumpable artifacts

The next stage is now implemented conservatively:

- each analyzed function computes a crate-local `UnsafeFunctionSummary`
- summaries distinguish:
  - direct unsafe-sensitive sinks
  - unknown-boundary effects
  - per-argument direct-sink vs propagation effects
- summaries can be inspected through:
  - `RZ_UNSAFE_DATAFLOW_SUMMARY_STATS=1`
  - `RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP=1`

Current dump format:

- one JSONL record per function
- default location:
  - `${CARGO_TARGET_DIR:-target}/rusteze-unsafe-summaries/<crate>.jsonl`

The summary model is intentionally coarse but now separates:

- `has_direct_sink`
- `calls_unknown_boundary`
- per-pointer-arg:
  - `direct_sink_mask`
  - `propagation_mask`

This makes it possible to inspect real builds before using summaries for optimization.

### Step 3 blocked for now: local same-session propagation needs a different implementation

A first attempt at same-session crate-local propagation was made, but it ran into rustc query-model
constraints:

- querying other local bodies through `optimized_mir` from inside the pass created query cycles
- precomputing summaries from `optimized_mir` in `after_analysis` stole MIR bodies before the
  custom pass could use them
- precomputing from `mir_for_ctfe` is invalid for non-const functions
- borrowing earlier `Steal<Body>`-based MIR in `after_analysis` is also not generally available,
  because some bodies are already stolen by that point

So crate-local interprocedural propagation is **not enabled** in the current implementation.

Current status:

- intra-procedural unsafe-sensitive analysis is active
- per-function summaries are active
- std/core/alloc external summary classification is active for selected APIs
- an analyze-only summary mode is active:
  - `RZ_ANALYZE_UNSAFE_SUMMARIES=1`
  - computes summaries and dumps/logs them without mutating MIR
- same-session crate-local propagation is deferred until we implement a summary pipeline that does
  not violate rustc's MIR query ownership model

The likely direction is still:

- a true two-stage local pipeline over a precomputed body cache, or
- the broader two-build summary/index design described earlier in this document

## Why this differs from the failed provenance-dataflow experiment

The failed local experiment reasoned about:
- pointer-value provenance equivalence

But the instrumentation consumes:
- tag locals
- ref-ancestor locals
- call-boundary tag transport

The new design avoids that mistake by using interprocedural dataflow for:
- deciding *where* metadata propagation is needed
- improving *which summaries* apply at call boundaries

It does **not** assume that two pointer carriers are interchangeable at every semantic hook.

## Implementation architecture

### Compiler-side pieces

1. `instrument-mir` analyze-only mode
- new CLI mode or env gate
- emits per-crate summaries

2. summary merge tool
- separate binary or script
- reads all crate summaries
- computes merged program index

3. guided instrumentation mode
- existing pass loads the merged index if present
- uses summaries conservatively

### Suggested files

- `instrument-mir/src/summary.rs`
  - summary structs + serialization
- `instrument-mir/src/summary_collect.rs`
  - crate-local summary extraction
- `instrument-mir/src/program_index.rs`
  - merged index format + lookup helpers
- `instrument-mir/src/bin/rusteze-merge-summaries.rs`
  - merge tool

This keeps whole-program logic separate from the current local instrumentation code path.

## Build integration

The easiest integration point is the existing cargo wrapper flow.

Suggested flow:

1. `cargo instrument-mir --analyze-only ...`
   - runs a first build
   - emits per-crate summaries

2. `rusteze-merge-summaries target/rusteze-summaries ...`
   - produces `program_index.json`

3. `cargo instrument-mir --use-program-index=... ...`
   - second build with real instrumentation

This can later be wrapped by a convenience script, but the underlying contract should stay explicit.

## Generic and trait-dispatch caveats

Exact whole-program precision is hard because of:
- generics
- trait dispatch
- function pointers
- proc-macro/build-script boundaries
- coroutine lowering

The first implementation should therefore:
- use conservative direct-call edges where known
- summarize unknown/indirect calls as escaping/unknown
- prefer over-approximation

It is acceptable for the first version to lose precision on dynamic dispatch, as long as it remains
sound.

## Validation plan

Validation must happen in stages.

### Stage 1: summary collection sanity
- summaries emitted for all instrumented crates
- merge succeeds on current target set
- missing/stale index cleanly falls back to current behavior

### Stage 2: functional soundness
- `python3 scripts/run_example_tests.py` fully green
- current smoke-fuzz targets (`bytes`, `quick_xml`, others) build and start fuzzing
- no new immediate false positives on existing corpora

### Stage 3: usefulness
- reduced propagation-hook count
- improved call/return modeling on real crates
- measurable overhead reduction on parser-heavy targets

## Recommended first milestone

Do not start with aggressive pruning.

Start with:
1. per-function summaries
2. merged whole-program unsafe/pointer-relevance propagation
3. better call/return summaries in the second build

Only after that should we use the index to prune metadata propagation hooks.

That sequencing keeps the first interprocedural version conservative and debuggable.
