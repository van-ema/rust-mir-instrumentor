use std::ptr;

// Purpose: stress-test as_ptr + arithmetic + call + volatile read in a tight loop.
// Expected: no tag=0, no garbage pointee addresses, fresh tags per derived pointer.
// Validates: unsize backtracking, PtrDerive placement, and pointer-local init fixes.

#[inline(never)]
unsafe fn callee(p: *const i32) -> i32 {
    ptr::read_volatile(p)
}

fn main() {
    let arr = [10i32, 20, 30, 40];
    let base = arr.as_ptr(); // from &[i32] unsize -> as_ptr backtrack path
    let mut sum = 0i32;
    for i in 0..100 {
        let idx = (i % arr.len()) as isize;
        unsafe {
            let p = base.add(1).sub(1).offset(idx);
            sum += callee(p);
        }
    }
    println!("sum={sum}");
}
