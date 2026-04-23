// Ported from miri/tests/fail/unaligned_pointers/promise_alignment.rs.
// Covers the `call_unaligned_ptr` revision.

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

    unsafe { utils::miri_promise_symbolic_alignment(align8.add(1).cast(), 8) };
}
