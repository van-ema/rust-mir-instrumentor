use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let p = &mut x as *mut u8;
    unsafe {
        let r1: &mut u8 = &mut *p;
        let r2 = &*p;
        let v = *r2;
        black_box(v);
        // r2 is used again below, so the shared borrow is still live across this write.
        *r1 = 3;
        // Violation should be reported on the subsequent read via r2.
        black_box(*r2);
    }
    black_box(x);
}
