// Purpose: demonstrate lineage loss when the common raw ancestor
// `v.as_mut_ptr().add(i)` is recomputed in optimized MIR instead of being kept
// in a stable local. Without SSA anchors, the two `&mut` creations below can
// become unrelated root-like reborrows and the later read through `r_ref` stays
// silent. With SSA anchors, both sites reuse the same raw ancestor tag and the
// second `&mut` disables the first one.

#[inline(never)]
fn write_through(x: &mut u8) {
    unsafe { std::ptr::write_volatile(x, 1) }
}

#[inline(never)]
fn read_through(x: &mut u8) -> u8 {
    *x
}

fn main() {
    let mut v = vec![0u8; 16];

    let r_ref: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };
    let s_ref: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };

    write_through(s_ref);
    let val = read_through(r_ref);
    println!("{val}");
}
