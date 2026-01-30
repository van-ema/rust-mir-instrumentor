use std::hint::black_box;

// Inspired by Miri SB tests that cover projections (field access).
fn main() {
    #[repr(C)]
    struct S {
        x: u8,
        y: u8,
    }

    let mut s = S { x: 0, y: 1 };
    let p = (&mut s.x) as *mut u8;

    unsafe {
        let r1: &mut u8 = &mut *p;
        let r2: &mut u8 = &mut *p;
        *r2 = 1;
        *r1 = 2; // UB under SB-lite: older unique used after a newer unique
    }

    black_box(s.y);
}

