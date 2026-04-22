// Ported from miri/tests/fail/dangling_pointers/dyn_size.rs.
//@compile-flags: -Zmiri-disable-stacked-borrows

struct SliceWithHead(#[allow(dead_code)] u8, #[allow(dead_code)] [u8]);

fn main() {
    let buf = [0u32; 1];
    let ptr: *const SliceWithHead = unsafe { std::mem::transmute((&buf, 4usize)) };
    let _ptr = unsafe { &*ptr };
}
