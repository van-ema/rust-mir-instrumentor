use core::ptr;

fn main() {
    let mut x = 42i32;
    let p: *mut i32 = (&mut x) as *mut i32;

    unsafe { ptr::write_volatile(p, 43); } // MIR tends to keep this as a call/side-effecting op
    println!("{x}");
}