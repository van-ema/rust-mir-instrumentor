# Bytes and SmallVec False Positives (2026-05-10)

## TODO

- [x] Fix `bytes` empty-view sentinel false positives on `addr=0x1`.
- [x] Fix `smallvec` recovered-shared helper arg misalignment false positives.
- [x] Fix `smallvec` same-family helper-read frozen-write false positives.

## 1. `bytes`: empty views lose lineage and fall back to a fresh `0x1` raw root

### Example

The current `bytes` crash set is all variants of safe empty-buffer plumbing. One representative
shape is:

```rust
use bytes::{Bytes, BytesMut};

let mut m = BytesMut::new();
let b = m.freeze();
let mut tail = b.clone();
let _ = tail.split_off(0);
```

Another equivalent shape is:

```rust
use bytes::BytesMut;

let mut m = BytesMut::new();
let spare = m.spare_capacity_mut();
assert!(spare.is_empty() || spare.len() >= 0);
```

In the harness these come from operations such as:

- `freeze`
- `split_off`
- `slice` / `slice_ref`
- `spare_capacity_mut`

See [afl_bytes_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_bytes_driver.rs:70).

### Current behavior

Today rusteze reports:

- `WILD_POINTER`
- `READ via raw derive addr=0x1 size=1`
- `reason=NO_PROVENANCE_DERIVE`

The runtime trace shows this shape:

1. `bytes` uses `0x1` as the aligned dangling sentinel for an empty view.
2. A helper path crosses a call boundary with `addr=0x1` and `exact=0`.
3. The callee falls back to a fresh raw root at `0x1`.
4. A later raw derive from that empty view is checked as if it were a concrete `size=1` read in
   [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs:686).
5. We report `NO_PROVENANCE_DERIVE`.

This is a false positive. No real dereference has happened. The problem is not that `bytes`
constructed an invalid pointer; the problem is that rusteze treated empty-view transport as a
fresh provenance event.

### Desired behavior after the principled patch

The empty view should keep the same lineage across helper transport.

So the intended behavior is:

1. an empty slice/view keeps the parent family if it comes from an existing empty family
2. boundary transport on that view must not fall back to a fresh raw root at `0x1`
3. raw derives over the same empty precise view are treated as metadata-only
4. the first non-empty access or non-empty ref creation remains fully checked

### Principled patch

The patch should be structural, not a blanket ignore:

1. preserve boundary lineage for empty precise views instead of exporting/taking `tag=0`
2. teach the runtime empty-view raw-derive path to accept:
   - same address
   - zero-length bounds
   - same live empty family
3. keep `NO_PROVENANCE_DERIVE` for real empty forgeries where there is no prior family to inherit

That follows the intended TB direction: an empty view is metadata, not an access.

## 2. `smallvec`: recovered shared helper arg is exported through a byte-aligned raw child

### Example

The minimized safe pattern is read-only:

```rust
use smallvec::SmallVec;

let mut v: SmallVec<u8, 8> = SmallVec::new();
let sum: u32 = v.iter().map(|&x| x as u32).sum();
let _ = sum;
```

or equivalently with one element:

```rust
use smallvec::SmallVec;

let mut v: SmallVec<u8, 8> = SmallVec::new();
v.push(8);
let _ = v.iter().count();
```

See [afl_smallvec_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_smallvec_driver.rs:77).

### Current behavior

Today rusteze reports:

- `MISALIGNED_ACCESS`
- `REF_CREATE via tag=... size=16`
- `required_alignment=8 guaranteed_alignment=1`

The trace shape is:

1. `v.iter()` goes through a read-only chain:
   - `Deref::deref`
   - `SmallVec::as_slice`
   - `SmallVec::as_ptr`
   - `RawSmallVec::as_ptr_inline`
2. The stable shared family is initially correct.
3. A recovered helper argument at the projected inline-buffer field is canonicalized through a
   byte-aligned raw child instead of a live ref-family projected descendant under the stable shared
   boundary parent.
4. The next ref reconstruction inherits `align=1`.
5. A later shared ref creation for the inline storage fails with `MISALIGNED_ACCESS`.

This is a false positive. The source path is read-only and safe. The bug is that the call-boundary
authority became a raw descendant that is valid only for byte-level observation, not for
reconstructing an aligned aggregate/shared view.

### Desired behavior after the principled patch

The helper chain should stay inside the stable shared family, but the exported tag still has to
match the projected callee address.

That means:

1. recovered shared helper args should export/import a live `RefShared`/`RefMut` projected
   descendant under the boundary parent when one exists
2. byte-aligned raw descendants may exist locally, but they must not become boundary authorities
3. reconstructed refs should inherit the aligned shared family, not the raw child

### Principled patch

The patch should tighten call-boundary canonicalization:

1. for recovered shared args at a projected address, prefer a live same-address ref-family
   descendant whose lineage contains the `boundary_parent`
2. only allow raw descendants to drive boundary transport when the semantic operation itself is
   raw-only
3. keep local exact/raw tags for in-body checking, but do not let them override the stable shared
   family on helper import/export

This is the same design principle as the boundary-parent work elsewhere in the runtime: export the
semantic family that crosses the boundary, not the last transient internal node.

## 3. `smallvec`: helper reads materialize durable shared siblings that freeze a later write

### Example

The minimized crashing shape is ordinary safe mutation:

```rust
use smallvec::SmallVec;

let mut v: SmallVec<u8, 8> = SmallVec::new();
v.pop();
v.insert(0, 4);
v.pop();
v.push(3);
```

In the harness this is just a mix of:

- `push`
- `pop`
- `insert`
- `iter_sum`

See [afl_smallvec_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_smallvec_driver.rs:28).

### Current behavior

Today rusteze reports:

- `TREE_BORROWS_VIOLATION`
- `WRITE via tag=... kind=RefMut`
- `reason=TB_LITE_FROZEN_WRITE`

The TB dump has the same bad shape as earlier helper-shared false positives:

1. a unique family is created for the `SmallVec` slot
2. helper calls such as `len` and related internal read-only plumbing create additional shared
   whole-slot nodes
3. those helper-created shared nodes remain live/frozen
4. the real write path for `insert` arrives later through a unique descendant
5. TB-lite rejects the write because the helper siblings are still freezing the slot

This is a false positive. Safe `SmallVec::insert` should not be blocked by administrative,
same-family helper reads.

### Desired behavior after the principled patch

Read-only helper plumbing should not leave durable whole-slot shared borrows behind unless a real
source-level shared borrow escapes.

So the intended behavior is:

1. metadata-only helpers like `len` stay scalar-only
2. same-family non-escaping helper reads may materialize transient local TB nodes, but killing
   the transient read-only node must also collapse the freeze it imposed on unique ancestors once
   no live read-only descendant still overlaps the range
3. the final write proceeds through the unique family without artificial frozen siblings

### Principled patch

The patch should extend the helper-collapse design:

1. treat metadata helpers as non-borrowing observations
2. collapse non-escaping same-family helper shareds at tag-kill time by restoring frozen unique
   ancestors to their lazy `Reserved`/`Active` permission when the helper left no live read-only
   descendants
3. keep real escaping shared borrows fully modeled, but stop promoting administrative helper reads
   into long-lived whole-slot freezes

This keeps TB-lite closer to the intended TB/Miri behavior: helper plumbing should not create extra
semantic borrows that freeze a later safe mutation.
