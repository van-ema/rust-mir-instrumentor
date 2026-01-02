fn main() {
    // Purpose: write then read through a raw pointer using deref syntax.
    // Expected: WRITE for *p = ... and READ for let v = *p.
    // Validates: MIR matching for deref stores and loads.
    let mut x = 0i32;
    let p = &mut x as *mut i32;
    unsafe {
        *p = 7;
        let v = *p;
        println!("v={v}");
    }
}
