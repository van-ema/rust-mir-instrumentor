#[no_mangle] // ensures the symbol name is not mangled
pub fn say_hello() {
    println!("Hello from dynamically loaded library!");
}
