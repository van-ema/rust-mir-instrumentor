# Bytes Fuzz Crashes (2026-05-10)

## TODO

- [x] Add minimized regression inputs for the live reproducing cases.
- [x] Fix tracked raw-owner aggregate field projection transport in `BytesMut::reserve_inner`.
- [x] Add a release-regression guard for `Bytes::slice`.
- [x] Re-run the green `bytes` fuzz regression corpus after the fixes.
- [ ] Investigate pending stale input `fuzz/regressions/bytes/pending/stale_fuzz_id000003.hex`.

## Classification rule

The uninstrumented harness accepting an AFL input is useful context, but it is not enough to
classify a `rusteze` report as a false positive.

For these cases, the false-positive assessment is based on the runtime trace and the memory-safety
shape of the target operation:

1. the public `bytes` operations reached by the harness are safe APIs;
2. the trace shows `rusteze` losing or misassigning provenance before a valid library access;
3. the reported tag does not correspond to the object actually being accessed.

A Miri replay is included below as the stronger cross-check. The analysis does not rely on the
plain harness result as proof; it uses it only as a sanity signal that the input is not immediately
crashing without instrumentation.

## Miri cross-check

The saved inputs were replayed through the same `afl_bytes_driver` under the repo-pinned Miri:

```text
nightly-2025-08-01-x86_64-unknown-linux-gnu
miri 0.1.0 (adcb3d3b4c 2025-07-31)
```

Commands used:

```sh
MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-backtrace=full" \
  cargo miri run -p afl_harness --features bytes_driver --bin afl_bytes_driver -- <input>

MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-tree-borrows -Zmiri-backtrace=full" \
  cargo miri run -p afl_harness --features bytes_driver --bin afl_bytes_driver -- <input>
```

Results:

| Input | Miri default | Miri Tree Borrows |
| --- | --- | --- |
| `id:000000,sig:06,src:000000,time:11651,execs:2117,op:havoc,rep:6` | pass | pass |
| `id:000001,sig:06,src:000000,time:26004,execs:5227,op:havoc,rep:8` | pass | pass |
| `id:000002,sig:06,src:000000,time:51735,execs:11240,op:havoc,rep:1` | pass | pass |
| `id:000003,sig:06,src:000004,time:77868,execs:15619,op:havoc,rep:4` | pass | pass |
| `id:000004,sig:06,src:000004,time:106981,execs:18288,op:havoc,rep:2` | pass | pass |

This supports the false-positive classification for the saved inputs. It does not prove that every
future input in the same fuzz bucket is safe; each new trace class still needs either a Miri
cross-check, trace analysis, or both.

## Repro bucket

Live inputs from `fuzz/out/bytes/default/crashes`:

- `id:000000,sig:06,src:000000,time:11651,execs:2117,op:havoc,rep:6`
- `id:000001,sig:06,src:000000,time:26004,execs:5227,op:havoc,rep:8`
- `id:000002,sig:06,src:000000,time:51735,execs:11240,op:havoc,rep:1`
- `id:000004,sig:06,src:000004,time:106981,execs:18288,op:havoc,rep:2`

`id:000003,sig:06,src:000004,time:77868,execs:15619,op:havoc,rep:4` did not reproduce on the
current instrumented binary. Treat it as stale unless it reappears.

The live cases split into two trace classes:

1. `id:000000..000002`: `UNKNOWN_TAG` in `BytesMut::reserve_inner`.
2. `id:000004`: `OUT_OF_BOUNDS` in `Bytes::slice`.

Regression inputs added:

- `fuzz/regressions/bytes/reserve_inner_shared_projection_id000000.hex`
- `fuzz/regressions/bytes/reserve_inner_shared_projection_id000001.hex`
- `fuzz/regressions/bytes/reserve_inner_shared_projection_id000002.hex`
- `fuzz/regressions/bytes/slice_range_carrier_id000004.hex`

The stale AFL input is preserved separately at
`fuzz/regressions/bytes/pending/stale_fuzz_id000003.hex`. It is not part of the green corpus yet:
on the current release harness it reaches a later `BytesMut::spare_capacity_mut` false-positive
shape where heap-buffer leaf provenance is still missing.

## 1. `BytesMut::reserve_inner`: projected `Shared.vec` access loses provenance

### Target shape

The harness reaches ordinary `BytesMut` reserve paths through operations in
[afl_bytes_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_bytes_driver.rs:28).

The failing library code is in
[bytes_mut.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes_mut.rs:609). The
important shape is:

```rust
let shared: *mut Shared = self.data;

if (*shared).is_unique() {
    let v = &mut (*shared).vec;
    let v_capacity = v.capacity();
    let ptr = v.as_mut_ptr();
}
```

`Shared` contains the backing `Vec<u8>` and metadata:

```rust
struct Shared {
    vec: Vec<u8>,
    original_capacity_repr: usize,
    ref_count: AtomicUsize,
}
```

### Runtime trace

Representative `id:000000` trace:

```text
[tag-create][ref] tag=249 parent=0 pointee=0x55f9b35d2650 kind=shared bounds=40
[call-tag] push arg=0 addr=0x55f9b35d2650 exact=249 boundary_parent=249 tag=249
[tag-create][ref] tag=250 parent=249 pointee=0x55f9b35d2650 kind=shared bounds=40
[tag-create][raw] tag=251 parent=250 pointee=0x55f9b35d2670 kind=const bounds=0
[tag-create][raw] tag=252 parent=251 pointee=0x55f9b35d2670 kind=const bounds=0
UNKNOWN_TAG
READ unknown tag=0 size=8
bytes::bytes_mut::BytesMut::reserve_inner
```

The base `Shared` object is tagged correctly. Rusteze creates further tags for a projected field
near `Shared + 0x20`, but the later `Vec` metadata read is checked with tag `0`.

The failing read is part of using the `Vec` field under `(*shared).vec`; it should inherit
provenance from the `Shared` raw/ref family. The target operation is not reading through a forged
or freed pointer. The error is that rusteze loses the field projection's structural parent.

### Example

The reduced memory-safety shape is:

```rust
struct SharedLike {
    vec: Vec<u8>,
    rc: usize,
}

struct Buf {
    data: *mut SharedLike,
}

unsafe fn reserve_like(b: &mut Buf) {
    let shared = b.data;          // tagged pointer to the aggregate
    let v = &mut (*shared).vec;   // field ref should derive from `shared`
    let _ = v.capacity();         // current rusteze trace can check this with tag 0
}
```

Under Tree Borrows/Miri semantics, creating a field reference from a valid raw pointer to the
aggregate does not erase provenance. The field projection is still tied to the aggregate allocation
and is checked as a descendant access. Rusteze should do the same: the field-local tag may be more
precise, but it must be in the same allocation family.

### Assessment

This is a rusteze false positive by trace analysis.

The likely bug class is a projection-parent transport bug for raw-owner aggregates:

1. a raw pointer field inside `BytesMut` carries the `Shared` allocation provenance;
2. an unsafe field projection creates a local view of `Shared.vec`;
3. the projection access is not assigned the inherited parent tag;
4. runtime validation sees tag `0` and reports `UNKNOWN_TAG`.

### Principled patch direction

Do not add an ignore for `bytes`.

Instead, field projections from raw-owner aggregates should use structural parent selection:

1. when projecting from `*mut Aggregate` / `*const Aggregate` to a field, derive the field tag from
   the aggregate pointer family;
2. keep the field offset/range precise when it is known;
3. fall back to the aggregate family only when field precision is unavailable;
4. continue reporting tag `0` for genuinely untracked or forged raw pointers.

### Implemented patch

The final repair is structural, not a runtime provenance-repair helper.

Two issues were fixed:

1. stack-backed pointer-shadow slots now keep an absolute mirror without the stack alloc epoch, so
   a later lookup that temporarily falls back from `Alloc` to `Abs` does not lose an already
   recorded pointer-field shadow;
2. slice iterator constructors are no longer treated as `Ignore`. `core::slice::<impl [T]>::iter`
   and `::iter_mut` now use `CarrierCopyArg0`, so returned iterator carriers restore their internal
   pointer leaves from `arg0` instead of re-rooting them as fresh raw tags.

This matches the actual false-positive shapes:

```text
&mut [u8; 32] --protected arg-->
  slice::iter_mut()
    returned IterMut raw leaves inherit arg0 leaf shadow
    later item write stays inside the protected lineage
```

instead of the old behavior:

```text
&mut [u8; 32] --protected arg-->
  slice::iter_mut()
    returned IterMut raw leaf gets no structural transport
    local raw fallback creates parent=0
    later item write conflicts with the still-protected arg family
```

This is closer to the Miri/TB direction: iterator/view construction is not a new borrow root. It
is another carrier for the same underlying family, so rusteze must preserve that family through the
returned aggregate's pointer leaves.

## 2. `Bytes::slice`: range/bound stack carrier is checked with the outer `Bytes` tag

### Target shape

The harness reaches `Bytes::slice` / `slice_ref` through operation `14` in
[afl_bytes_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_bytes_driver.rs:75).

The failing library code is in
[bytes.rs](/home/ubuntu/rust-mir-instrumentor/third_party/bytes/src/bytes.rs:369).

`Bytes` is a 32-byte stack value on this target:

```rust
pub struct Bytes {
    ptr: *const u8,
    len: usize,
    data: AtomicPtr<()>,
    vtable: &'static Vtable,
}
```

### Runtime trace

Representative `id:000004` trace:

```text
[tag-create][ref] tag=168 parent=0 pointee=0x7fff0bccc920 kind=shared bounds=32
...
[tag-create][ref] tag=243 parent=... pointee=0x7fff0bccc938 kind=shared bounds=16
...
[ptr-shadow] load_tag slot=... -> 168
[ptr-shadow] sanitize slot=0x7fff0bccc920 value=0x7fff0bccc938 tag=168
...
[ptr-shadow] load_tag slot=... -> 168
[ptr-shadow] sanitize slot=0x7fff0bccc928 value=0x7fff0bccc940 tag=168
OUT_OF_BOUNDS
READ via tag=168 addr=0x7fff0bccc940 size=8
origin_alloc_base=0x7fff0bccc920 origin_alloc_end=0x7fff0bccc940 origin_alloc_size=32
bytes::bytes::Bytes::slice
```

`tag=168` covers the outer 32-byte `Bytes` stack object:

```text
0x...c920 .. 0x...c940
```

The trace also creates a separate 16-byte stack object at `0x...c938`, likely a range/bound carrier:

```text
0x...c938 .. 0x...c948
```

The failing read at `0x...c940` is one-past-end for the `Bytes` object, but it is in-bounds for the
16-byte range/bound carrier. The runtime is validating the carrier read using the outer `Bytes`
tag, so it reports a false out-of-bounds access.

### Example

The reduced memory-safety shape is:

```rust
struct BytesLike {
    ptr: *const u8,
    len: usize,
    data: usize,
    vtable: usize,
} // 32 bytes

fn slice_like(b: &BytesLike, range: core::ops::Range<usize>) {
    let end = range.end;
}
```

After optimization, a temporary range/bound carrier can be placed so that it overlaps the tail of
the stack area used near `b`:

```text
BytesLike object:       base + 0  .. base + 32
range/bound carrier:   base + 24 .. base + 40
read carrier word:     base + 32
```

`base + 32` is invalid for the `BytesLike` object, but valid for the range/bound carrier. A
correct dynamic checker must validate the read with the carrier's own stack family, not with the
borrow family of `&Bytes`.

### Assessment

This is a rusteze false positive by trace analysis.

The target operation is not dereferencing outside the `Bytes` object. It is reading a different
stack temporary whose lifetime and range are separate from the caller-visible `Bytes` borrow. The
reported OOB appears because rusteze reuses the outer `Bytes` tag for a carrier-local read.

### Tree Borrows / Miri comparison

Miri does not have a separate shadow slot map that must guess which pointer-sized stack value owns
which tag. It tracks borrows through its memory/provenance model. A range/bound temporary has its
own stack allocation/provenance, so a read from that temporary is not checked against the `&Bytes`
object's bounds.

Rusteze must approximate that behavior at MIR-instrumentation time. The principled approximation is
to assign stack/range carriers their own local family and avoid transporting the outer receiver
tag onto unrelated scalar or carrier reads.

### Current vs desired behavior

Example:

```rust
let bytes: Bytes = ...;
let end = bytes.len() - 1;
let out = bytes.slice(1..end);
```

Lowered `slice` does roughly this:

```rust
let range_ref = &_2;                  // &_2 is the local `Range<usize>` carrier
let start = RangeBounds::start_bound(range_ref);
match start {
    Bound::Included(p) | Bound::Excluded(p) => {
        let i = *p;
    }
    Bound::Unbounded => { ... }
}
```

Old behavior:

1. `start_bound` / `end_bound` were classified as `Ignore`.
2. The returned `Bound<&usize>` local received no structural shadow import from `&_2`.
3. The later `shadow_load_tag` on `((_5 as Included).0: &usize)` could therefore pick up stale
   stack shadow for that slot.
4. If the stale shadow belonged to a nearby `Bytes` helper/receiver family, the subsequent read of
   `*p` was checked as if it were a read through `&Bytes`, producing the bogus OOB.

Desired behavior:

1. treat `start_bound` / `end_bound` as structural carrier copies from `arg0`;
2. import the returned `Bound<&usize>` leaf shadow from `&_2`;
3. keep the later `*p` read in the local range/bound family rather than whatever old shadow was in
   that stack slot.

### Principled patch direction

Do not widen the `Bytes` tag and do not ignore `Bytes::slice`.

Instead, fix stack-local family selection:

1. distinguish receiver-object families from range/bound carrier families;
2. avoid using a caller-visible receiver tag for unrelated stack temporaries;
3. when an optimized temporary overlaps another stack slot, validate by the temporary's own live
   allocation/range metadata;
4. keep OOB reporting for real reads past the end of the actual object being accessed.

### Current status

`slice_range_carrier_id000004.hex` is now in the green release regression corpus. The broad
receiver-vs-carrier parent-selection reorder that would have targeted this directly was tested and
rejected because it can incorrectly make protected helper/carrier tags authoritative for ordinary
receiver-family accesses.

The retained fix is local: `RangeBounds::{start_bound,end_bound}` now use
`CallEffect::CarrierCopyArg0`, so the returned `Bound<&usize>` carrier imports shadow from the
range-object receiver instead of floating as an untracked helper result. The regression guard stays
because this was a real observed fuzz input, but the patch no longer depends on globally
reordering `ParentSelectionMode::PointeeFamily`.
