use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let p = &mut x as *mut u8;
    unsafe {
        let r1 = &mut *p;
        let r2 = &*p;
        let v = *r2;
        black_box(v);
        *r1 = 3; // should violate: write after shared reborrow
    }
    black_box(x);
}
