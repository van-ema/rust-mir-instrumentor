// Purpose: raw pointer argument propagates a nonzero tag into the callee.
// Expected: callee deref triggers a READ with tag != 0.
// Validates: call-argument tag propagation and deref READ instrumentation.

fn callee(p: *const i32) -> i32 {
    unsafe { *p } // should log READ with tag != 0
}

fn main() {
    let x = 10i32;
    let p = &x as *const i32;
    let v = callee(p);
    println!("v={v}");
}
