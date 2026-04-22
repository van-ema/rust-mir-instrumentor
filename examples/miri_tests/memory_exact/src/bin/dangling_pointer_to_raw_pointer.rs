// Ported from miri/tests/fail/dangling_pointers/dangling_pointer_to_raw_pointer.rs.
use std::ptr;

fn direct_raw(x: *const (i32, i32)) -> *const i32 {
    unsafe { &raw const (*x).0 }
}

fn via_ref(x: *const (i32, i32)) -> *const i32 {
    unsafe { &(*x).0 as *const i32 }
}

fn main() {
    let ptr = ptr::without_provenance(0x10);
    let _ = direct_raw(ptr);
    let _ = via_ref(ptr);
}
