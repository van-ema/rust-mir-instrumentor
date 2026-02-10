// Inspired by miri/tests/fail/tree_borrows/reserved/int-protected-write.rs.
// A write through a raw child while a call-argument `&mut` protector is active
// should be rejected by Tree Borrows.
fn write_second(x: &mut u8, y: *mut u8) {
    unsafe {
        *y = 0;
    }
    let _ = x;
}

fn main() {
    let n = &mut 0u8;
    let x = unsafe { &mut *(n as *mut u8) };
    let y = (&mut *n) as *mut u8;
    write_second(x, y);
}
