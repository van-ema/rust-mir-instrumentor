fn main() {
    let p: *mut u8 = 0x12345usize as *mut u8;
    unsafe { *p = 1; }
}
