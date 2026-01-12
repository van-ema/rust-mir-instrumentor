fn main() {
    let p = 0x12345usize as *mut u8;
    unsafe { *p = 1; }
}
