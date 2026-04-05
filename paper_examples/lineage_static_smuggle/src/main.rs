// Purpose: demonstrate that pointer-shadow memory preserves lineage when a raw
// pointer is stored in memory and later reloaded through a static slot.
//
// This is adapted from Miri's `tests/fail/stacked_borrows/pointer_smuggling.rs`.
// In Miri, storing a raw pointer in memory and loading it back preserves the
// provenance carried by that pointer value. Rusteze now does the same for this
// case: the static slot keeps shadow provenance alongside the pointer bits, so
// the later load restores the original lineage.
//
// Expected in --release: TREE_BORROWS_VIOLATION|WRITE|RawMut|1

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
    // Rusteze restores that same provenance from the static slot shadow, so
    // the write through `s` invalidates `r` as well.
    let _val = *r;
    println!("val={_val}");
}
