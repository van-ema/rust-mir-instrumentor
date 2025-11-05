pub fn hello() {
    let x = 42;
    let y = &x; // This creates a reference -> should trigger instrumentation
    println!("{}", y);
}
