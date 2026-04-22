// Ported from miri/tests/fail/dangling_pointers/deref_dangling_box.rs.
//@compile-flags: -Zmiri-disable-stacked-borrows

use std::ptr::{self, addr_of_mut};

fn main() {
    let mut inner = ptr::without_provenance::<i32>(24);
    let outer = addr_of_mut!(inner).cast::<Box<i32>>();
    let _val = unsafe { addr_of_mut!(**outer) };
}
