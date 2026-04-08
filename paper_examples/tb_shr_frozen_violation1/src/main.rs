// Purpose: import a Miri both-borrows case that exercises tb_lite's frozen
// state transition.
//
// Adapted from Miri:
// `tests/fail/both_borrows/shr_frozen_violation1.rs`
//
// After creating a shared reference from `&mut x`, an unknown raw mutable write
// through that shared path is forbidden while the location remains frozen.
//
// Expected in --release: TREE_BORROWS_VIOLATION|WRITE|RawMut|4

#![allow(invalid_reference_casting)]

fn foo(x: &mut i32) -> i32 {
    *x = 5;
    unknown_code(&*x);
    *x
}

fn main() {
    println!("{}", foo(&mut 0));
}

fn unknown_code(x: &i32) {
    unsafe {
        *(x as *const i32 as *mut i32) = 7;
    }
}
