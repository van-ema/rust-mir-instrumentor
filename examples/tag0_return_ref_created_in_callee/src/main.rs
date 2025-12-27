use std::ptr;

#[inline(never)]
unsafe fn callee(p: *const i32) -> *const i32 {
    let r: &i32 = unsafe { &*p };
    println!("callee ref = {}", *r);
    r as *const i32
}

fn main() {
    let x = 99i32;
    let p: *const i32 = &x as *const i32;
    let q = unsafe { callee(p) };
    let v = unsafe { ptr::read_volatile(q) };
    println!("value = {}", v);
}
