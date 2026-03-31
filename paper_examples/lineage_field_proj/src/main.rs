// Purpose: demonstrate lineage loss through a raw pointer field projection.
//
// When a struct holds a raw pointer field and a method returns it, the
// instrumentation sees a field projection (*self).ptr. The PtrDerive hook
// fires at the call site trying to link the result to `self`'s tag, but
// `self` is a &mut Wrapper (RefMut over the struct), not over the u8 buffer.
// The returned tag ends up linked to the struct allocation, not the buffer —
// or with parent=0 if the field projection chain breaks.
//
// Both calls to get_ptr() return the same raw *mut u8. Each call produces a
// tag with parent derived from self's tag (the Wrapper allocation), not from
// the buffer's provenance. If the two returned tags both resolve to parent=0
// (or to a detached struct tag that doesn't match the buffer allocation),
// they appear as root-vs-root siblings in the TB tree.
//
// TB-Lite: root-vs-root creation exception → no eager invalidation → false negative.
// Miri: both carry alloc_id of the buffer → same tree → foreign-write fires → UB.
//
// Expected: ok (TB-Lite false negative — lineage lost through field projection)

struct Wrapper {
    ptr: *mut u8,
    _len: usize,
}

impl Wrapper {
    fn get_ptr(&self) -> *mut u8 {
        // Returns (*self).ptr — a field projection.
        // Instrumentation: PtrDerive hook at the call site in the caller,
        // trying to link result to self's tag. But self points to Wrapper,
        // not to the buffer — the tag will be for the Wrapper slot, not u8 buffer.
        self.ptr
    }
}

fn main() {
    let mut buf = [0u8; 16];
    let w = Wrapper { ptr: buf.as_mut_ptr(), _len: 16 };

    // Two calls to get_ptr() — both return the same raw pointer.
    // Each call produces a new tag. Whether their parents are correctly linked
    // to the buffer's provenance depends on whether the field projection is tracked.
    let p = w.get_ptr();
    let q = w.get_ptr();

    let r: &mut u8 = unsafe { &mut *p };
    let s: &mut u8 = unsafe { &mut *q };

    // Only read through r — no writes.
    // Miri: r and q share buf's alloc_id → S disables R → UB.
    // TB-Lite: if parent lost via field proj → root-vs-root → missed.
    let _val = *r;
    let _ = s;
    println!("val={_val}");
}
