// Ported from miri/tests/fail/provenance/int_copy_looses_provenance0.rs.

use std::mem;

fn main() {
    let ptrs = [(&42, true)];
    let ints: [(usize, bool); 1] = unsafe { mem::transmute(ptrs) };
    let ptr = (&raw const ints[0].0).cast::<&i32>();
    let _val = unsafe { *ptr.read() };
}
