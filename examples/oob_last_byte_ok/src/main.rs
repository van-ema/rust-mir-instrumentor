fn main() {
    let mut v = vec![0u8; 8];
    let p = v.as_mut_ptr();
    unsafe {
        *p.add(7) = 1; // last byte, OK
    }
}
