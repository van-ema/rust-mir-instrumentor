use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;
    let r = &x;
    let raw = std::ptr::from_ref(r);

    let r2 = &mut x;
    black_box(r2);

    unsafe {
        let v = *raw; // should violate: raw read after newer unique
        black_box(v);
    }
}
