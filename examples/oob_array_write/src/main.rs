fn main() {
    let mut a = [0u8; 8];
    let p = a.as_mut_ptr();
    unsafe {
        *p.add(8) = 1; // OOB write by 1
    }
}
