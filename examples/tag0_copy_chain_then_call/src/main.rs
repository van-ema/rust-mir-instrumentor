use std::ptr;

#[inline(never)]
unsafe fn callee(p: *const i32) {
    let v = unsafe { ptr::read_volatile(p) };
    println!("callee read = {}", v);
}

fn main() {
    let x = 5i32;
    let p0: *const i32 = &x as *const i32;
    let p1 = p0;
    let p2 = p1;
    unsafe { callee(p2) };
    println!("done");
}
