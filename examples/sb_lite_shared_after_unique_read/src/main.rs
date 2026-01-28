use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let p = &mut x as *mut u8;
    unsafe {
        let r1 = &*p;
        let r2 = &mut *p;
        *r2 = 4;
        let v = *r1; // should violate: read through old shared after unique
        black_box(v);
    }
    black_box(x);
}
