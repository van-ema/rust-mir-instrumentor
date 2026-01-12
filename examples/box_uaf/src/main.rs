// USE_AFTER_DEAD
fn main() {
    let b = Box::new(0u8);
    let p = Box::into_raw(b); // p: *mut u8, allocation still live
    unsafe {
        // free
        drop(Box::from_raw(p));
        // UAF write
        *p = 1;
    }
}
