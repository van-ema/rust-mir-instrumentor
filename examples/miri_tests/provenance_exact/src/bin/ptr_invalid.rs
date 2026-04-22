// Ported from miri/tests/fail/provenance/ptr_invalid.rs.

fn main() {
    let x = 42;
    let xptr = &x as *const i32;
    let xptr_invalid = std::ptr::without_provenance::<i32>(xptr.expose_provenance());
    let _val = unsafe { *xptr_invalid };
}
