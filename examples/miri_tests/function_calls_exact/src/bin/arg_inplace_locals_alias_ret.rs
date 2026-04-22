// Ported from miri/tests/fail/function_calls/arg_inplace_locals_alias_ret.rs.
//@compile-flags: -Zmiri-tree-borrows -Zmiri-disable-validation

#![feature(custom_mir, core_intrinsics)]

use std::intrinsics::mir::*;

#[allow(unused)]
pub struct S(i32);

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        let _unit: ();
        {
            let staging = S(42);
            let _non_copy = staging;
            Call(_non_copy = callee(Move(_non_copy)), ReturnTo(after_call), UnwindContinue())
        }
        after_call = {
            Return()
        }
    }
}

fn callee(x: S) -> S {
    x
}
