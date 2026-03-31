// Purpose: demonstrate lineage loss when intermediate MIR locals are optimized away.
//
// In debug builds the chain:
//   _p = v.as_mut_ptr()        → tag P, parent = Vec internal buf tag
//   _q = _p.add(i)             → tag Q, parent = P
//   _r = &mut *_q              → tag R, parent = Q
//   _s = &mut *_q              → tag S, parent = Q  ← S is child of Q, lineage ok
// In optimized builds the temporaries _p and _q are fused away:
//   _r = &mut *v.as_mut_ptr().add(i)   → src_place has no intermediate local
//   _s = &mut *v.as_mut_ptr().add(i)   → same
//   Both R and S get parent=0 → root-vs-root siblings.
//
// TB-Lite: root-vs-root creation exception fires → R not disabled at S creation.
// Only reads through R → R stays Reserved → no violation reported (false negative).
// Miri: S creation fires foreign-write on R (same allocation tree) → R Disabled
//       → read through R is UB.
//
// Expected: ok (TB-Lite false negative — lineage lost via optimized-away locals)

fn make_two_refs(v: &mut Vec<u8>, i: usize) -> (*mut u8, *mut u8) {
    // Force explicit intermediate locals so the compiler has something to fuse away.
    // In --release these will likely be inlined into a single expression in MIR.
    let p = v.as_mut_ptr();
    let q = unsafe { p.add(i) };
    let r = q;
    let s = q;
    (r, s)
}

fn main() {
    let mut v = vec![0u8; 16];
    let (r, s) = make_two_refs(&mut v, 4);

    // Create two &mut from the two raw pointers — both should get parent=0 in
    // optimized MIR because the intermediate locals were fused.
    let r_ref: &mut u8 = unsafe { &mut *r };
    let s_ref: &mut u8 = unsafe { &mut *s };

    // Only read through r_ref. Never write through either.
    // TB-Lite: r_ref is Reserved (S's creation didn't disable it) → no violation.
    // Miri: r_ref is Disabled (S's creation fired foreign-write) → UB.
    let _val = *r_ref;
    let _ = s_ref;
    println!("val={_val}");
}
