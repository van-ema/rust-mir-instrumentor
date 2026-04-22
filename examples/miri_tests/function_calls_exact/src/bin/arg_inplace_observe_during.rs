// Ported from miri/tests/fail/function_calls/arg_inplace_observe_during.rs.
//@compile-flags: -Zmiri-tree-borrows

#![feature(custom_mir, core_intrinsics)]

use std::intrinsics::mir::*;

pub struct S(i32);

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        let _unit: ();
        {
            let non_copy = S(42);
            let ptr = std::ptr::addr_of_mut!(non_copy);
            Call(_unit = change_arg(Move(*ptr), ptr), ReturnTo(after_call), UnwindContinue())
        }
        after_call = {
            Return()
        }
    }
}

#[expect(unused_variables, unused_assignments)]
fn change_arg(mut x: S, ptr: *mut S) {
    x.0 = 0;
    unsafe { ptr.read() };
}
