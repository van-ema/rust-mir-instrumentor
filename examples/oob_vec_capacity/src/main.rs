// OUT_OF_BOUNDS (provenance-aware classification should kick in if tag derives from base alloc).
fn main() {
    let mut v: Vec<u8> = Vec::with_capacity(8);
    let p = v.as_mut_ptr();
    unsafe {
        *p.add(8) = 0xAA; // OOB write by 1 (past capacity)
    }
}
