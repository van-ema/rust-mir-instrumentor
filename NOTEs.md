

# Notes

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