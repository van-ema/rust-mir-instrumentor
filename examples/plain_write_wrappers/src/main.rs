use std::hint::black_box;
use std::ptr;

fn main() {
    // Purpose: exercise ptr::write and ptr::write_unaligned wrappers.
    // Expected: WRITE for both calls (not just USE).
    // Validates: plain wrapper classification for aligned and unaligned stores.
    let mut x = 0u32;
    let p = &mut x as *mut u32;
    unsafe {
        ptr::write(p, 0x5566_7788);
    }

    let mut buf = [0u32; 2];
    let base = buf.as_mut_ptr() as *mut u8;
    let unaligned = unsafe { base.add(1) } as *mut u32;
    unsafe {
        ptr::write_unaligned(unaligned, 0x1122_3344);
    }

    black_box(x);
    black_box(buf);
}
