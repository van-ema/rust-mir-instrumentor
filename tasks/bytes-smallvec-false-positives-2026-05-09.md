# Bytes and SmallVec False Positives (2026-05-09)

## TODO

- [x] Fix zero-length raw/view sentinel false positives in `bytes`.
- [x] Fix same-family shared return export false positives in `smallvec`.

## 1. `bytes`: zero-length view over an aligned dangling sentinel

### Example

The harness can reach this shape through safe operations such as:

```rust
use bytes::BytesMut;

let mut buf = BytesMut::with_capacity(8);
buf.clear();

let spare = buf.spare_capacity_mut();
assert!(spare.len() == 8);
```

and:

```rust
use bytes::Bytes;

let mut b = Bytes::new();
let _tail = b.split_off(0);
```

The live repros are:

- `fuzz/regressions/bytes/bytesmut_empty_spare_capacity_mut.hex`
- `fuzz/regressions/bytes/empty_bytes_split_off.hex`

### Current behavior

Today rusteze reports:

- `WILD_POINTER`
- `READ via raw derive addr=0x1 size=1`
- `reason=NO_PROVENANCE_DERIVE`

The root issue is in the strict raw-derive path in [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs:686). We still model some zero-length raw/view creation as if it were a real 1-byte read. For `bytes`, that is too strong: empty buffers legitimately use an aligned dangling sentinel such as `0x1`.

So the current model is effectively:

1. construct an empty raw/view value
2. immediately treat that construction as `READ size=1`
3. reject the sentinel as missing provenance

That is a false positive because no concrete dereference has happened yet.

### Desired behavior after the principled patch

For zero-length raw/view construction:

1. keep the view family / lineage
2. keep alignment checks
3. do **not** perform a synthetic 1-byte provenance read
4. defer provenance and OOB enforcement to the first non-empty access or ref creation

So the same example should behave like this:

```text
BytesMut::spare_capacity_mut() on empty buffer
  -> create zero-length raw/view metadata
  -> no alias/provenance violation yet
  -> later non-empty write/read still checked normally
```

This is the Tree Borrows / Miri direction: an aligned dangling pointer used only to represent an empty view is not itself a memory access.

## 2. `smallvec`: read-only helper chain exports a transient exact shared tag

### Example

The smallest live reproducer is just:

```rust
use smallvec::SmallVec;

let mut v: SmallVec<u8, 8> = SmallVec::new();
v.push(8);
let sum: u32 = v.iter().map(|&x| x as u32).sum();
let _ = sum;
```

In the harness this is the `iter_sum` branch in [afl_harness/src/bin/afl_smallvec_driver.rs](/home/ubuntu/rust-mir-instrumentor/afl_harness/src/bin/afl_smallvec_driver.rs:77).

The safe library path is:

```rust
v.iter()
  -> Deref::deref(&v)
  -> SmallVec::as_slice(&v)
  -> &[T] iterator plumbing
  -> shared element reads
```

Relevant target code:

- [third_party/smallvec/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/third_party/smallvec/src/lib.rs:2033)
- [third_party/smallvec/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/third_party/smallvec/src/lib.rs:1535)

### Current behavior

Today rusteze reports:

- `TREE_BORROWS_VIOLATION`
- `RET invalid ref tag=... kind=RefShared`
- `reason=TB_LITE_INVALIDATED`

raised from [runtime/src/lib.rs](/home/ubuntu/rust-mir-instrumentor/runtime/src/lib.rs:5112).

The representative trace shape is:

```text
stable shared family over `&v`         tag=205
helper-local shared exact child        tag=206
another helper-local shared exact child tag=207
projected shared for returned view     tag=215
RET validates exact tag 215
tag 215 is already disabled
```

So the current model does this:

1. create a stable shared family for the read-only receiver
2. materialize extra exact shared nodes for helper returns in the same family
3. let one of those helper-local exact nodes become invalidated by later helper plumbing
4. export and validate that exact node at the return boundary

This is a false positive. The whole call chain is a read-only same-family helper path. The boundary should be attached to the stable shared family, not to a transient exact helper tag.

### Desired behavior after the principled patch

The return export should use the same-family canonical parent, not the transient exact tag.

So the same example should behave like this:

```text
`&v` shared family created
  -> `as_slice()` / `iter()` stay in that family
  -> helper-local exact tags may exist internally
  -> return/export canonicalizes back to the stable shared family
  -> boundary validation checks the stable live shared family
  -> no violation
```

Concretely, the principled patch is:

1. when a returned shared view/ref is a same-family projection of a live receiver-family borrow,
   export the canonical boundary parent instead of the exact helper-local tag
2. keep exact helper-local tags for local checking
3. avoid making those helper-local same-family shareds caller-visible boundary authorities

That aligns the return path with the call-boundary parent design already used for recovered call arguments: the exported tag should represent the stable semantic family that crosses the boundary, not the last transient internal helper node.

In the live `SmallVec::as_slice` failure, that boundary-export issue combined with one more transport bug:

- generic `*const T` returns such as `SmallVec::as_ptr` / `RawSmallVec::as_ptr_inline` were treated as
  non-address-exposable, so they skipped `RetPush` / `RetTake`
- summary-based `PtrDerive` on instrumented `&self -> *const T` wrappers was then too coarse: it modeled
  the result as derived from the receiver pointer itself rather than from the callee's returned raw tag

The principled fix is therefore:

1. treat thin generic pointer returns as address-exposable when the pointee is `Sized` in the current
   typing environment
2. let those calls use normal return-boundary transport
3. reject summary-based `PtrDerive` for instrumented wrappers that take `&T` to a non-pointer pointee,
   so boundary transport remains authoritative for `SmallVec::as_ptr`-style helpers
