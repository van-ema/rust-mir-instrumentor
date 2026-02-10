∆

# Notes

## Access-size computation via MIR size_of

To avoid `layout_of` normalization failures in generic MIR, access sizes are now
emitted using MIR `size_of::<T>()` for sized types. This means size computation
is deferred to codegen/runtime and we no longer query layouts during
instrumentation. Unsized types or non-thin pointers still fall back to size=0
as a conservative unknown.

## Wide (fat) pointers: use the data pointer address

Wide pointers (`&[T]`, `&str`, `dyn Trait`) carry metadata in addition to the
data address, and `PointerExposeProvenance` expects a pointer value.

To keep tag/epoch tracking keyed by a concrete address, the instrumentor
extracts a **thin** data pointer first:

- thin pointers: `addr = expose_provenance(ptr)`
- wide pointers: `thin = (ptr as *const ()/*mut ())` then `addr = expose_provenance(thin)`

This drops the metadata (length / vtable) on purpose for address identity: the
runtime’s allocation map is keyed by the data address. Metadata length is still
recorded separately in tag metadata for slice/str bounds checks.

## Slice/str bounds via metadata length

When a tag is created from a wide pointer, the instrumentor now passes the
metadata length (slice length in bytes / str length) to the runtime. The runtime
stores this as `TagMeta.bounds_len` and checks reads/writes against
`[pointee_addr, pointee_addr + bounds_len)` in addition to allocation bounds.

This improves detection of forged or mismatched metadata. We now compute
projection-based offsets for deref reads/writes (field/index/subslice), so
index-based OOB is detected when the access stays in a single MIR place.
Offsets can still be missed when pointer arithmetic is performed in a separate
local/call or when projections use `from_end`/nested deref patterns.

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

## Return-tag recovery must update ref-ancestor locals

Caller-side return recovery (`InstrKind::RetTake`) writes the recovered tag into
the destination tag local. After introducing ref-ancestor-aware parent lowering,
that was not sufficient: `PtrDerive` prefers the destination's ref-ancestor
local when selecting `derived_from`.

Bug pattern:

- `RetTake` updated `tag_local(dst)` but left `ref_ancestor_local(dst)` at `0`.
- Later pointer derivation (e.g. `Vec::as_ptr().add(n)`) used parent `0`.
- The derived tag lost provenance and reads/writes were classified as
  `WILD_POINTER` instead of provenance-aware `OUT_OF_BOUNDS`.

Fix:

- `RetTake` now also initializes `ref_ancestor_local(dst)` from the recovered
  destination tag before control returns to the original call target block.

Outcome:

- OOB examples like one-past-end deref through returned pointers now keep
  lineage and classify as `OUT_OF_BOUNDS` instead of `WILD_POINTER`.

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

## Access address precision for deref projections

For deref reads/writes, we now compute the **actual accessed address** by
walking projections after the first `Deref`:

- `Field` offsets are derived from layout when available.
- `Index`/`ConstantIndex`/`Subslice` offsets use element size × index.

This fixes common cases like `(*p).field`, `slice[i]`, and enables slice-length
OOB detection with wide-pointer metadata.

Remaining limitations (best-effort):
- Pointer arithmetic done in separate locals/calls (e.g. `p.add(i)` then write)
  may still use the base address if MIR does not encode the offset in the place.
- Projections with `from_end` or nested deref chains are not modeled yet.

## Runtime parent-allocation inheritance: detached-parent fallback

Runtime tag creation (`__record_ref_creation`) normally inherits allocation
epoch/live snapshots from the parent tag when `parent_tag != 0`. This preserves
stale-pointer detection across reborrows.

However, if parent metadata is already detached from any current allocation and
we continue inheriting it, we can produce cascading false positives (notably
spurious OOB/UAF in wrapper-heavy optimized code paths).

Current fallback:

- If parent and pointee resolve to concrete allocations, we keep/refresh
  metadata based on base/epoch compatibility (existing behavior).
- If parent resolves but pointee does not and pointee is clearly outside parent
  range, we break lineage for this tag (`parent=0`, epoch/live reset).
- If parent does not resolve but pointee does, we refresh from pointee alloc.
- If neither resolves and parent carried nonzero epoch metadata, we break
  lineage to avoid propagating stale allocation snapshots.

This keeps true OOB/UAF examples detectable while reducing false positives from
detached ancestry.

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

## Tree Borrows lite protectors at call boundaries

`tb_lite` now models a lightweight protector rule for call arguments:

- when the callee consumes a caller-pushed parent tag (`__rz_take_call_arg_tag`),
  the runtime records that parent in a call frame;
- the immediate child `Ref*` tag created from that parent is marked as protected;
- the frame is popped on callee return (`__rz_exit_fn`), inserted at each MIR
  `Return` terminator.

Checks implemented in TB-lite:

- `TB_LITE_PROTECTOR_CONFLICT`: overlapping write via a different tag while a
  protected tag is active;
- `TB_LITE_PROTECTOR_DEALLOC`: heap deallocation of an allocation that still has
  an active protected tag.

## Tree Borrows lite permission state machine

`tb_lite` now tracks an explicit per-tag permission state:

- `Reserved(conflicted=false/true)`
- `Active`
- `Frozen`
- `Disabled`

Current transitions (node-level, range-overlap based):

- New `RefMut` starts `Reserved(conflicted=false)`; write through it promotes to `Active`.
- New `RawMut` starts `Active`.
- New shared/raw-const tags start `Frozen`.
- Unique/raw-mut writes disable overlapping non-ancestor branches.
- Foreign reads over protected `Reserved(conflicted=false)` set
  `Reserved(conflicted=true)`.
- Child writes through protected `Reserved(conflicted=true)` are rejected
  (`TB_LITE_2PHASE_CONFLICT`).
- Foreign reads over `Active` degrade to `Frozen` (or `Disabled` if protected).
- Foreign writes disable the overlapping node.
- `Disabled` tags are treated as invalidated.

This is still intentionally lite compared to full Miri Tree Borrows: transitions
are modeled at tag/range granularity (not per-byte location state), and some
protector/2-phase details remain to be implemented.

## AFL++ on macOS (shared memory)

On this macOS setup, AFL++ shared-memory initialization can fail due to OS restrictions:

- SysV shared memory: `shmget()` / `shmat()` failures (often “Invalid argument” or attach failure)
- POSIX shared memory: `shm_open()` can fail with `EPERM` depending on sandboxing / policies

Workaround: run AFL++ on Linux (native, VM, or Docker). For a Docker workflow that is
useful for crash reproduction and minimization, see `docker/afl/README.md`.
