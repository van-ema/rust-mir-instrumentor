unsafe fn callee(p: *const i32) { std::ptr::read(p); }

fn main() {
    let x = 1;
    let p = &x as *const i32;
    unsafe { callee(p.wrapping_offset(0)); }
}