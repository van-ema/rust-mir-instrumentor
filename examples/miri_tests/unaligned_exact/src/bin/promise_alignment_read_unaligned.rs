// Ported from miri/tests/fail/unaligned_pointers/promise_alignment.rs.
// Covers the `read_unaligned_ptr` revision.

#[path = "../../utils/mod.rs"]
mod utils;

#[repr(align(8))]
#[derive(Copy, Clone)]
struct Align8(#[allow(dead_code)] u64);

fn main() {
    let buffer = [0u32; 128];
    let buffer = buffer.as_ptr();
    unsafe { utils::miri_promise_symbolic_alignment(buffer.cast(), 1) };
    let _val = unsafe { buffer.read() };

    let align8 = if buffer.addr() % 8 == 0 {
        buffer
    } else {
        buffer.wrapping_add(1)
    };
    assert!(align8.addr() % 8 == 0);
    unsafe { utils::miri_promise_symbolic_alignment(align8.cast(), 8) };
    unsafe { utils::miri_promise_symbolic_alignment(buffer.cast(), 1) };
    let _val = unsafe { align8.cast::<Align8>().read() };

    #[repr(align(16))]
    #[derive(Copy, Clone)]
    struct Align16(#[allow(dead_code)] u128);

    let align8_not16 = if align8.addr() % 16 == 8 {
        align8
    } else {
        align8.wrapping_add(2)
    };
    assert_eq!(align8_not16.addr() % 16, 8);

    let _val = unsafe { align8_not16.cast::<Align16>().read() };
}
