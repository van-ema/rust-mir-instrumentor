# Toml Fuzz False-Positive Investigation

This note records the process used to classify the two toml AFL crashes found after
`7340f91 Fix stale leaf shadow transport`.

## Inputs

Crash directory:

```bash
fuzz/out/toml/default/crashes
```

Observed inputs:

```text
id:000022,sig:06,src:000182,time:96797,execs:9765,op:havoc,rep:2
id:000023,sig:06,src:000182,time:113993,execs:11379,op:havoc,rep:1
```

Raw bytes:

```bash
xxd -g 1 -c 32 fuzz/out/toml/default/crashes/<input>
```

Results:

```text
id:000022: 22
id:000023: 27 27 27 27 51 2f 00 3e
```

## Replay

First replay each input against the current instrumented AFL driver:

```bash
TARGET=toml PROFILE=release AFL_REPRO_STRICT=1 \
RZ_ALIAS_MODEL=tb_lite RZ_INSTRUMENT_ALL_DEPS=1 \
scripts/afl_repro.sh --input fuzz/out/toml/default/crashes/<input>
```

Both inputs reproduced on the current binary, so they were not stale AFL artifacts.

Important detail: the AFL harness uses `@@` file-path mode. In
`afl_harness::read_input_with_limit`, an argv path is read with `std::fs::read`.
Running the same bytes through stdin may exercise a different allocation path and can miss the
crash.

## Trace

Capture runtime traces on the same argv/file-path path:

```bash
RZ_ALIAS_MODEL=tb_lite \
RUSTEZE_FAILFAST=1 \
RZ_ABORT_ON_VIOLATION=1 \
RZ_INSTRUMENT_ALL_DEPS=1 \
RZ_LOG_LOC=1 \
RZ_TRACE_CALL_TAGS=1 \
RZ_TRACE_PTR_SHADOW=1 \
RZ_TRACE_TAG_CREATE=1 \
RZ_TB_TRACE=1 \
./target/afl-release-toml/release/afl_toml_driver \
  fuzz/out/toml/default/crashes/<input> \
  > /tmp/toml.trace 2>&1
```

Useful trace filters:

```bash
rg -n "STALE_RETURN_REF_LEAF_SHADOW|CALL_ARG invalid|TB_LITE_INVALIDATED" /tmp/toml.trace
rg -n "tag=<tag>|node tag=<tag>|pointee=0x<addr>|ptr=0x<addr>" /tmp/toml.trace
```

## Miri Check

Run the same harness and input under Miri:

```bash
MIRIFLAGS=-Zmiri-disable-isolation \
cargo miri run -p afl_harness --features toml_driver --bin afl_toml_driver -- \
  fuzz/out/toml/default/crashes/<input>
```

`-Zmiri-disable-isolation` is needed because the harness reads the crash file from disk via
`std::fs::read`. It allows filesystem access for the test; it does not disable Miri's aliasing or
provenance checks.

Both inputs passed under Miri, which supports classifying these as rusteze false positives rather
than target memory bugs.

## Classification

### `id:000022`

Rusteze report:

```text
WILD_POINTER
RET stale ref leaf shadow
reason=STALE_RETURN_REF_LEAF_SHADOW
```

Trace finding:

- The failing slot contained stale shadow metadata.
- The concrete pointer value still had valid exact ref tags.
- Those exact tags described precise empty views (`bounds=0`).
- The pointer was an empty one-past-style view, so a live allocation snapshot may not exist for the
  exact pointer address.

Conclusion:

This is a false positive. Empty views carry provenance, but validating or returning them should not
perform a byte read.

Implemented patch:

- Return-leaf recovery accepts exact, TB-valid, precise-empty ref tags even when
  `lookup_alloc_snapshot(ptr)` fails.
- Non-empty refs stay on the stricter live-allocation path.

Regression:

```text
fuzz/regressions/toml/basic_string_empty_view_return_id000022.hex
```

### `id:000023`

Rusteze report:

```text
TREE_BORROWS_VIOLATION
CALL_ARG invalid ref tag=492
reason=TB_LITE_INVALIDATED
```

Trace finding:

- `tag=492` was an older shared ref over the input buffer.
- TB-lite had already disabled it.
- The same concrete input allocation had newer live shared tags for overlapping/current views.
- Pointer shadow later re-exported the old disabled tag as a call-boundary fact.

Conclusion:

This is a false positive. The target program still has valid shared provenance; rusteze transported
stale shadow metadata across a call boundary.

Implemented patch:

- Before exporting call-boundary shadow, require the selected ref tag to be a valid TB boundary
  fact.
- If the exact shadow tag is invalidated, recover a live ref tag for the same concrete pointer value
  or same live allocation view, but only with the same reference kind (`&T` stays shared,
  `&mut T` stays mutable).
- If no valid ref tag exists, keep validation on the original tag so a real dead-provenance export
  still reports instead of being hidden.

Regression:

```text
fuzz/regressions/toml/basic_string_stale_call_arg_boundary_id000023.hex
```

## General Rule

Classify a fuzz crash as a rusteze false positive only after both checks hold:

1. The same input is accepted by Miri for the same harness path, or the trace proves the target flow
   is memory-safe under the intended TB/Miri model.
2. The rusteze trace shows a metadata transport/recovery error, such as stale shadow, dead boundary
   tag export, or overly strict empty-view recovery.
