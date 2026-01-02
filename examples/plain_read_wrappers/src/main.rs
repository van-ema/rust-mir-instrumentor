use std::hint::black_box;
use std::ptr;

fn main() {
    // Purpose: exercise ptr::read and ptr::read_unaligned wrappers.
    // Expected: READ for both calls (not just USE).
    // Validates: plain wrapper classification for aligned and unaligned loads.
    let x = 0x1020_3040u32;
    let p = &x as *const u32;
    let v = unsafe { ptr::read(p) };

    let buf = [0xA1B2_C3D4u32, 0x1122_3344u32];
    let base = buf.as_ptr() as *const u8;
    let unaligned = unsafe { base.add(1) } as *const u32;
    let w = unsafe { ptr::read_unaligned(unaligned) };

    black_box(v);
    black_box(w);
}
