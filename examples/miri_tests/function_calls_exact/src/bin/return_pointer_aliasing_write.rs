// Ported from miri/tests/fail/function_calls/return_pointer_aliasing_write.rs.
//@compile-flags: -Zmiri-tree-borrows

#![feature(core_intrinsics)]
#![feature(custom_mir)]

use std::intrinsics::mir::*;

#[custom_mir(dialect = "runtime", phase = "optimized")]
fn main() {
    mir! {
        {
            let _x = 0;
            let ptr = &raw mut _x;
            Call(_x = myfun(ptr), ReturnTo(after_call), UnwindContinue())
        }

        after_call = {
            Return()
        }
    }
}

fn myfun(ptr: *mut i32) -> i32 {
    unsafe { ptr.write(0) };
    13
}
