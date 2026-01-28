use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;

    let raw = {
        let r1 = &mut x;
        &raw mut *r1
    };

    let r2 = &mut x;
    black_box(r2);

    unsafe {
        *raw = 1; // should violate: raw write after newer unique
    }
}
