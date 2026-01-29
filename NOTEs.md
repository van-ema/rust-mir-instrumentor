∆

# Notes

## Access-size computation via MIR size_of

To avoid `layout_of` normalization failures in generic MIR, access sizes are now
emitted using MIR `size_of::<T>()` for sized types. This means size computation
is deferred to codegen/runtime and we no longer query layouts during
instrumentation. Unsized types or non-thin pointers still fall back to size=0
as a conservative unknown.

## Unknown-call allow-untagged reads/writes

When we see a direct call that is unclassified and not instrumented, we insert
conservative pointer effects for each pointer argument:

- `PtrReadAllowUntagged` and `PtrWriteAllowUntagged` are emitted at the call site.
- They lower to `__rz_ptr_read_allow_untagged` / `__rz_ptr_write_allow_untagged`.
- The runtime returns early when `tag == 0`, so missing metadata does not
  raise `UNKNOWN_TAG` while still checking nonzero tags.

These hooks are only used for the unknown-call policy, not for ordinary deref
reads/writes or classified wrappers.

## ArgRetag ordering at function entry

The callee-side ArgRetag sequence (`__rz_take_call_arg_tag` then
`__record_ref_creation` / `__record_raw_ptr_creation`) must run before any
instrumented read/write that uses the argument's tag local.

We enforce this with two layers of ordering:

- `InstrKind::ArgRetag` has highest priority (same bucket as ref/raw/root) so
  it sorts ahead of reads/writes at the same `stmt_idx`.
- ArgRetag insert points are applied **after** all other instrumentation in
  `insert_instrumentation`. Because insertion walks points in reverse and each
  insertion rewrites the entry terminator, the *last-applied* ArgRetag executes
  *first* at runtime.

Example (SmallVec::len):

- Before: `__rz_ptr_read(tag_local, addr, size)` ran before
  `__rz_take_call_arg_tag(...)`, so `tag_local` was 0 and raised UNKNOWN_TAG.
- After: entry order is `__rz_take_call_arg_tag(...)` →
  `__record_ref_creation(...)` → `__rz_ptr_read(...)`, so the tag is initialized
  before the read.

This is the necessary plumbing to avoid UNKNOWN_TAG on plain reads of `&self`
in instrumented dependencies like `smallvec`.

### Instrumentation priority order

When multiple hooks target the same basic block and statement index, we order
by `instr_priority` before insertion. The buckets are:

1) Priority 0: `Ref`, `Raw`, `RawRoot`, `ArgRetag`, `RetRoot`, `PtrDerive`
2) Priority 1: `PtrRead`, `PtrWrite` (and allow-untagged variants)
3) Priority 2: `CallArgPush`, `PtrUse`
4) Priority 3: everything else (alloc/lifetime hooks, ret push/take, etc.)

Why this order:

- Creation/derivation first (priority 0) ensures tags exist before any access.
- Reads/writes next (priority 1) should observe the tag produced by creation.
- Escape/side-effect bookkeeping after that (priority 2) should see the tag
  state after any direct access.
- Allocation/lifetime bookkeeping last (priority 3) avoids reordering caller
  control flow around accesses and keeps entry/exit hooks as outer wrappers.

We still rely on the "reverse insertion" rule when splitting terminators:
later-applied hooks execute earlier at runtime. The explicit priorities keep
the ordering stable when multiple hooks share the same insertion point.

## PtrWrite address precision caveat

Right now, the `PtrWrite` instrumentation computes the write address as:

- `addr = expose_provenance(ptr_local)`

This is the **pointer value** stored in the base pointer local (e.g., `_p`), turned into a `usize`.

### What this catches well

This is correct for *simple deref writes* where the write happens at the pointer's base address:

```rust
unsafe { *p = 43; }
```

In MIR terms, this is essentially a store to `(*p)` with no additional offset.

### What it does NOT capture yet

For *interior writes*, the actual store address is **base + offset**, but we currently only pass the base pointer value.

Examples:

```rust
unsafe { (*p).field = 1; }      // field offset
unsafe { p.add(3).write(7); }   // pointer arithmetic
unsafe { slice[i] = 9; }        // indexing
```

In MIR these show up as a `Deref` plus additional projections (e.g., `Field`, `Index`, etc.). The real accessed address depends on the projection chain.

### TODO to improve later

Compute the **actual accessed address** for a deref write by accounting for projections after `Deref`:

- walk `lhs_place.projection` after the first `Deref`
- use type/layout info to compute field offsets
- handle indexing and pointer arithmetic
- then pass `addr = base + computed_offset` to `__rz_ptr_write`

This will be necessary to correctly track writes like `(*p).field = ...` and other interior accesses.

## Drop glue and implicit address-of

MIR drop terminators (`TerminatorKind::Drop`) call drop glue with a pointer to
the local being dropped. This effectively takes `&place` even when there is no
explicit `Rvalue::Ref`/`Rvalue::RawPtr` in the statements. To avoid missing
stack allocations that are only touched by drop glue, we treat `Drop` as an
implicit address-of when computing the set of stack locals worth tracking.

## Static metadata reads (vtables) via raw pointers

Optimized MIR often loads function pointers from vtables or other static
metadata via raw-pointer reads. These addresses live in the program image
(\_\_TEXT/\_\_DATA/\_\_DATA\_CONST on macOS) and are not tracked by the heap/stack
allocation map, so naive range lookup reports WILD_POINTER.

Current behavior (macOS + Linux): the runtime enumerates loaded image segments
via dyld on macOS and via `dl_iterate_phdr` on Linux, treating their ranges as
static allocations. Reads inside these segments are allowed; writes are allowed
only if the segment is writable. Writes into read-only static segments report
WRITE_TO_READONLY_STATIC.

Limitations:
- OS-specific (macOS + Linux; other targets fall back to no static ranges).
- Coarse: we do not recover embedded pointer provenance; we only check address
  ranges for static segments.
