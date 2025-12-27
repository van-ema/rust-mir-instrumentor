use std::ptr;

#[inline(never)]
unsafe fn callee(p: *const u8) {
    let v = unsafe { ptr::read_volatile(p) };
    println!("callee byte = {}", v);
}

fn main() {
    let x: u32 = 0x1122_3344;
    let p: *const u32 = &x as *const u32;
    let q: *const u8 = p as *const u8;
    unsafe { callee(q) };
    println!("done");
}
