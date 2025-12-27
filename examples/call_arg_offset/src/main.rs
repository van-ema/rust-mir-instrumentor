unsafe fn callee(p: *const i32) { unsafe { std::ptr::read_volatile(p); } }

fn main() {
    let x = 1;
    let p = &x as *const i32;
    unsafe { callee(p.offset(0)); } // arg becomes an rvalue / projection-like case
}
