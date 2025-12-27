unsafe fn callee(p: *const i32) { unsafe { std::ptr::read_volatile(p); } }

fn main() {
    let x = 1;
    let p = &x as *const i32;
    let q = p as *const i32; // temp/cast propagation
    unsafe { callee(q); }
}
