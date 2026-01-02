// Purpose: pass derived pointers into callee and deref there.
// Expected: no tag=0 in callee; derived tags preserved across call boundaries.
// Validates: pointer arithmetic + call retagging (CallArgPush/ArgTake).
fn callee(p: *const i32) -> i32 {
    unsafe { *p }
}

fn main() {
    let arr = [10i32, 20, 30, 40];
    let base = arr.as_ptr();
    unsafe {
        let p = base.add(0).add(1).sub(1); // wrappers may appear; should still be tagged
        let v = callee(p);
        println!("callee read = {v}");
    }
}
