// examples/stack_reuse.rs
use std::ptr;

fn main() {
    let p: *const i32;

    {
        let x: i32 = 111;
        p = &x as *const i32;
    } // x dies, epoch increments

    // New scope; likely reuses stack slot (not guaranteed, but common in optimized builds)
    {
        let y: i32 = 222;
        unsafe { ptr::read_volatile(&y); } // keep y "used"
    }

    unsafe {
        let v = ptr::read_volatile(p);
        println!("v = {}", v);
    }
}