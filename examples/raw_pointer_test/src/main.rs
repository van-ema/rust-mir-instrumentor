fn main() {
    let x = 42;
    let r = &x; // Create a reference
    let raw_ptr = r as *const i32; // Create a raw pointer from the reference
    unsafe {
        println!("Value through raw pointer: {}", *raw_ptr);
    }
}
