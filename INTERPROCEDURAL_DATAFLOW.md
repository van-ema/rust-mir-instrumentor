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
