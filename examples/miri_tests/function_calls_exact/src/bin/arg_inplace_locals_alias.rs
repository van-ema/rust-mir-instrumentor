// Ported from miri/tests/fail/function_calls/arg_inplace_locals_alias.rs.
//@compile-flags: -Zmiri-tree-borrows -Zmiri-disable-validation

#![feature(custom_mir, core_intrinsics)]

use std::intrinsics::mir::*;

pub struct S(i32);

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        let _unit: ();
        {
            let staging = S(42);
            let non_copy = staging;
            Call(_unit = callee(Move(non_copy), Move(non_copy)), ReturnTo(after_call), UnwindContinue())
        }
        after_call = {
            Return()
        }
    }
}

#[expect(unused_variables, unused_assignments)]
fn callee(x: S, mut y: S) {
    y.0 = 0;
    assert_eq!(x.0, 42);
}
