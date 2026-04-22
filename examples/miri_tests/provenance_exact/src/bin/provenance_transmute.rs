// Ported from miri/tests/fail/provenance/provenance_transmute.rs.
//@compile-flags: -Zmiri-permissive-provenance

#![allow(integer_to_ptr_transmutes)]

use std::mem;

unsafe fn deref(left: *const u8, right: *const u8) {
    let left_int: usize = mem::transmute(left);
    let right_int: usize = mem::transmute(right);
    if left_int == right_int {
        let left_ptr: *const u8 = mem::transmute(left_int);
        let _val = *left_ptr;
    }
}

fn main() {
    let ptr1 = &0u8 as *const u8;
    let ptr2 = &1u8 as *const u8;
    unsafe {
        deref(ptr1, ptr2.with_addr(ptr1.addr()));
    }
}
