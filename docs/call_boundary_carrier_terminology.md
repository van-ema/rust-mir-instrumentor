# Call-Boundary Carrier Terminology

## Carrier

A carrier is a non-pointer value that contains pointer fields inside it.

Example:

```rust
struct Buf {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}
```

`Buf` itself is not pointer-typed, but `Buf::ptr` is a pointer inside it.

## Pointer Leaf

A pointer leaf is one concrete pointer field inside a carrier.

For `Buf`, the pointer leaf is:

```rust
buf.ptr
```

For a nested carrier:

```rust
struct Outer {
    inner: Buf,
}
```

the pointer leaf is:

```rust
outer.inner.ptr
```

## Leaf Shadow

A leaf shadow is runtime metadata attached to one pointer leaf. It records the provenance of the
pointer stored in that field.

Conceptually:

```text
slot address of buf.ptr -> {
  tag: 42,
  ref_ancestor: 40,
  export_parent: 40,
  recovered: false,
}
```

When a carrier is copied by value across a function call, rusteze can transport the shadow for the
internal pointer leaf too. That lets the callee recover the provenance of the pointer field without
inventing a whole-carrier borrow.

## Pointer-Leaf Shadow

Pointer-leaf shadow is the same idea as leaf shadow, but the name makes clear that the leaf is a
pointer field.

## Anchor

An anchor is a local tag slot for the whole non-pointer carrier value.

Example:

```rust
fn f(buf: Buf) {
    let r = &buf;
}
```

`buf` is not pointer-typed, so it does not have a normal pointer tag local. But code can still
borrow the whole carrier with `&buf` or `&mut buf`. Rusteze stores a stable family tag for that
carrier stack slot in the carrier's anchor.

Conceptually:

```text
anchor(buf) = tag for the borrow family of the whole buf slot
```

## Anchor Seed

An anchor seed is the value used to initialize a carrier anchor.

For raw-owner carriers with exactly one structural pointer leaf, rusteze can seed the callee's
carrier anchor from the restored pointer-leaf shadow:

```text
caller pushes shadow for buf.ptr
callee restores shadow for callee_buf.ptr
callee seeds anchor(callee_buf) from callee_buf.ptr shadow
```

So "anchor seed from shadow" means:

```text
anchor(buf) = shadow tag loaded from buf.ptr
```

This avoids creating a fake whole-carrier call-boundary borrow when the real provenance is carried
by the internal pointer field.

## Example Flow

```rust
struct Buf {
    ptr: *mut u8,
    len: usize,
}

fn callee(b: Buf) {
    let p = b.ptr;
    let r = &b;
}
```

Caller side:

```text
b.ptr has pointer tag 10
leaf shadow for b.ptr = tag 10
call callee(b)
```

Call boundary:

```text
push leaf shadow: arg0 leaf ptr has tag 10
```

Callee entry:

```text
restore leaf shadow into callee_b.ptr
seed anchor(callee_b) from callee_b.ptr shadow
```

After that:

```text
callee_b.ptr uses tag 10
&callee_b can derive from anchor(callee_b), in the same family
```

The important rule is that rusteze preserves the structural pointer provenance already inside the
value instead of inventing unrelated whole-carrier provenance.
