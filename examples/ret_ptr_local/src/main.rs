#[inline(never)]
fn callee() -> *const i32 {
    let x = 123;
    let r = &x;
    // return raw pointer derived from a reference created inside callee
    r as *const i32
}

#[inline(never)]
fn use_ptr(p: *const i32) -> i32 {
    unsafe { *p }
}

fn main() {
    let p = callee();
    let v = use_ptr(p);
    println!("v={}", v);
}