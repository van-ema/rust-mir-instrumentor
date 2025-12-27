use std::ptr;

#[inline(never)]
unsafe fn callee(p: *const i32) -> i32 {
    let v = unsafe { ptr::read_volatile(p) };
    println!("callee read = {}", v);
    v
}

fn main() {
    let x = 7i32;
    let p: *const i32 = &x as *const i32;
    let v = unsafe { callee(p.wrapping_offset(0)) };
    println!("main saw = {}", v);
}
