// Ported from miri/tests/fail/provenance/strict_provenance_cast.rs.
//@compile-flags: -Zmiri-strict-provenance

fn main() {
    let addr = &0 as *const i32 as usize;
    let _ptr = std::ptr::with_exposed_provenance::<i32>(addr);
}
