use std::ptr;

fn main() {
    // Purpose: exercise ptr::write_volatile and ptr::read_volatile wrappers.
    // Expected: volatile READ/WRITE (or USE) logged per current support.
    // Validates: wrapper recognition and volatile handling.
    let mut x = 0i32;
    let p = &mut x as *mut i32;
    unsafe {
        ptr::write_volatile(p, 5);
        let v = ptr::read_volatile(p);
        println!("v={v}");
    }
}
