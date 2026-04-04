// Purpose: demonstrate lineage loss when a raw pointer is stored in memory and
// later reloaded through a static slot.
//
// This is adapted from Miri's `tests/fail/stacked_borrows/pointer_smuggling.rs`.
// In Miri, storing a raw pointer in memory and loading it back preserves the
// provenance carried by that pointer value. In Rusteze today, the static slot
// only stores the bits of the pointer. When the value is reloaded, its lineage
// is detached from the original borrow tree.
//
// Pointer-shadow memory would fix this by storing provenance alongside the
// pointer bits in the static slot and restoring it at the later load.
//
// Expected in --release: ok (TB-Lite false negative — pointer provenance lost
// when smuggled through static memory)

static mut PTR: *mut u8 = std::ptr::null_mut();

fn stash(x: &mut u8) {
    unsafe { PTR = x; }
}

fn main() {
    let mut val = 0u8;
    let r = &mut val;
    stash(r);

    *r = 2;

    let p = unsafe { PTR };
    let s: &mut u8 = unsafe { &mut *p };
    unsafe { std::ptr::write_volatile(s, 3) };

    // Miri: the raw pointer loaded from PTR still carries provenance derived
    // from `r`, so the write through `s` invalidates `r`.
    // Rusteze today: PTR stores only the pointer bits. Reloading `p` loses the
    // lineage, so `s` does not invalidate `r` and the read succeeds.
    let _val = *r;
    println!("val={_val}");
}
