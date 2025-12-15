
fn main() {
    let mut x = 42;
    let r = &mut x; // This creates a mutable reference -> should trigger instrumentation
    *r += 1; // Modify the value through the mutable reference
    println!("x = {}", x);
}