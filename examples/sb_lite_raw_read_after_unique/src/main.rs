use std::hint::black_box;

fn main() {
    let mut x: u8 = 0;

    let raw = {
        let r_shared = &x;
        &raw const *r_shared
    };

    let r_unique = &mut x;
    black_box(r_unique);

    unsafe {
        let v = *raw; // should violate: raw read after newer unique
        black_box(v);
    }
}
