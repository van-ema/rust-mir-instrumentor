use std::hint::black_box;

// Inspired by Miri SB tests exercising "old unique after new unique" on a
// projection (array element).
fn main() {
    let mut a = [0u8, 1u8];
    let p = (&mut a[0]) as *mut u8;

    unsafe {
        let r1: &mut u8 = &mut *p;
        let r2: &mut u8 = &mut *p;
        *r2 = 10;
        *r1 = 11; // UB: older unique used after a newer unique
    }

    black_box(a);
}
