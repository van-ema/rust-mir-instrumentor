// Purpose: demonstrate that generic pointee types cause alias_exempt=true,
// completely disabling TB-Lite alias checks for those tags.
//
// When T has an unresolved type parameter at MIR instrumentation time,
// the instrumentation cannot determine if T is Freeze, what its layout is,
// or whether aliasing rules apply. It conservatively sets alias_exempt=true
// for all tags derived from *mut T / *const T / &T / &mut T.
//
// With alias_exempt=true:
// - No TB node is created for the tag
// - No alias model check fires on read or write
// - The alias violation is invisible to TB-Lite regardless of access pattern
//
// Miri: T has a concrete type at every interpretation step → full TB applies.
//
// Expected: ok (TB-Lite is completely blind — alias_exempt bypasses all checks)

unsafe fn alias_through_generic<T: Copy>(p: *mut T) -> T {
    // Create two &mut T from the same raw pointer.
    // T is generic → alias_exempt=true → no TB node, no check.
    let r: &mut T = &mut *p;
    let s: &mut T = &mut *p;

    // Writing through s then reading through r: textbook aliasing UB.
    // TB-Lite: alias_exempt → no violation reported.
    // Miri: T is concrete at runtime → two aliasing &mut → S disables R → UB.
    *s = *r; // read r, write s
    *r       // read r after s wrote to it
}

fn main() {
    let mut x = 42u64;
    let val = unsafe { alias_through_generic(&mut x as *mut u64) };
    println!("val={val}");
}
