use std::hint::black_box;

// Ported (simplified) from Miri `fail/both_borrows/alias_through_mutation.rs`.
//
// This mutates a `&u32` (shared) to point at a location that is also written
// through a `&mut u32`, creating an invalid aliasing pattern.
fn retarget(x: &mut &u32, target: &mut u32) {
    unsafe {
        // Retarget the shared reference to point at `target` via a raw-pointer cast.
        *x = &mut *(target as *mut _);
    }
}

fn main() {
    let mut v: u32 = 42;
    let target: &mut u32 = &mut v;

    let mut target_alias: &u32 = &0; // dummy initial value
    retarget(&mut target_alias, target);

    // Mutate through the unique borrow, then read through the shared alias.
    *target = 13;
    black_box(*target_alias); // UB: read via invalidated shared ref
}
