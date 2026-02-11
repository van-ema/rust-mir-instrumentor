// Case: callee returns heap raw pointer, allocation is dropped/replaced, old ptr is used.
// Goal: stress stale epoch tracking across call return boundaries.

#[inline(never)]
fn take_ptr(v: &mut Vec<u8>) -> *mut u8 {
    v.as_mut_ptr()
}

fn main() {
    let mut v = vec![1u8; 32];
    let p = take_ptr(&mut v);

    let replacement = vec![2u8; 32];
    drop(v);

    unsafe {
        *p = 9;
    }

    std::hint::black_box(replacement);
}
