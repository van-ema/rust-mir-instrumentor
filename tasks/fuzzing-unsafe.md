# Fuzzing Unsafe Targets

## Goal

Use `rusteze` to fuzz real-world Rust crates with unsafe internals and maximize the chance of
finding:
- memory-safety bugs
- alias-model violations
- instrumentation/runtime false positives worth fixing

Main harness policy:
- prefer stateful public-API harnesses
- focus on memory-safety and aliasing findings first
- do not require a semantic oracle unless it is necessary for the target
- if an oracle hurts throughput or creates harness-side noise, omit it

## Target Order

Priority order for unsafe-target fuzzing:
1. `smallvec`
2. `bytes`
3. `rkyv`
4. `hashbrown`
5. `bumpalo`
6. `indexmap`

## General Heuristics

For each crate, prefer harnesses that stress:
- repeated reborrows and wrapper-heavy public APIs
- stale-view / stale-pointer style state transitions
- resize / reserve / split / merge / freeze / thaw style invalidation edges
- helper-heavy code paths that tend to lose provenance in optimized MIR
- public APIs that exercise deep unsafe internals without requiring unrealistic setup

Use operation-sequence inputs rather than one-shot parsers when possible.

## Crate Notes

### `smallvec`

Focus on:
- inline-to-heap and heap-to-inline transitions
- `insert`, `remove`, `swap_remove`, `drain`, `retain_mut`, `append`
- repeated spill / shrink / re-grow cycles

Status:
- main harness exists

### `bytes`

Focus on:
- `BytesMut` / `Bytes` ownership transitions
- `freeze`, `try_into_mut`, `split_to`, `split_off`, `unsplit`
- shared-slice and stale-view hypotheses

Status:
- main harness exists
- prefer the no-oracle fast harness

### `rkyv`

Focus on:
- archive / access / deserialize / mutate / reuse cycles
- checked and unchecked access on serializer-produced bytes
- malformed archived buffers and nested archived structures

Status:
- harness exists

### `hashbrown`

Focus on:
- raw table mutation paths
- `entry`, `entry_ref`, `raw_entry`, `raw_entry_mut`
- `reserve`, `drain`, `extract_if`, `retain`, `get_disjoint_mut`
- wrapper-heavy raw-entry and insertion paths

Status:
- harness exists

### `bumpalo`

Focus on:
- bump-allocation growth and chunk rollover
- slice-building helpers and allocation-after-reset patterns
- stale references across reset-like transitions

Status:
- pending

### `indexmap`

Focus on:
- map order maintenance under insert/remove/swap/remove_entry style operations
- mutation during iteration-like public APIs
- interactions between index maintenance and underlying hash table storage

Status:
- pending

## Workflow

For each target:
1. build a stateful harness around public APIs
2. seed it with small operation-sequence corpora
3. run `rusteze` and ASan
4. classify crashes as:
   - real target bug
   - instrumentation/runtime false positive
   - harness bug/noise
5. fix false positives at the root cause
6. add a regression if the false positive is important or recurring
