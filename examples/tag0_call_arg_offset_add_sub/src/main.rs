use std::ptr;

#[inline(never)]
unsafe fn callee(p: *const i32) {
    let v = unsafe { ptr::read_volatile(p) };
    println!("callee read = {}", v);
}

fn main() {
    let arr = [10i32, 20, 30];
    let p = arr.as_ptr();
    unsafe {
        callee(p.add(0));
        callee(p.sub(0));
        callee(p.offset(0));
    }
    println!("done");
}
