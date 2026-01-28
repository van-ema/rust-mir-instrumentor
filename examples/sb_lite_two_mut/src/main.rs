use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let p = &mut x as *mut u8;
    unsafe {
        let r1 = &mut *p;
        let r2 = &mut *p;
        *r2 = 1;
        *r1 = 2; // should violate: old unique after new unique
    }
    black_box(x);
}
