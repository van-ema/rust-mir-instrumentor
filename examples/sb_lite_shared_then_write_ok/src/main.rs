use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let p = &mut x as *mut u8;
    unsafe {
        let r1: &mut u8 = &mut *p;
        let r2 = &*p;
        let v = *r2;
        black_box(v);
        // r2's last use is above, so NLL ends the shared borrow before this write.
        *r1 = 3;
    }
    black_box(x);
}
