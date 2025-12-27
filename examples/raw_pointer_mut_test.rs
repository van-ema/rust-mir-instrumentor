fn main() {
    let mut x = 42i32;

    let raw_ptr: *mut i32 = &raw mut x; // Create a mutable raw pointer directly
    unsafe {
        *raw_ptr += 1;
        println!("Value through mutable raw pointer: {}", *raw_ptr);
    }

    println!("x after raw mutation: {}", x);
}
