// Purpose: demonstrate why pointer-shadow metadata must survive bytewise copies
// of pointer-carrying objects.
//
// This is inspired by Miri's provenance tests in `tests/pass/provenance.rs`,
// especially the bytewise memcpy cases. Miri preserves the provenance of a
// pointer value when it is copied through memory byte-by-byte.
//
// Here the raw pointer is first stored inside `Slot`, then the entire `Slot`
// object is copied bytewise into `dst`. The later field loads `dst.ptr` must
// recover the same pointer provenance as `src.ptr`; otherwise the reloaded
// pointers lose their common ancestor and TB-Lite misses the sibling
// invalidation.
//
// Expected in --release: TREE_BORROWS_VIOLATION|WRITE|RawMut|1

use std::mem::MaybeUninit;

#[derive(Copy, Clone)]
struct Slot {
    ptr: *mut u8,
}

#[inline(never)]
fn read_ref(x: &mut u8) -> u8 {
    *x
}

unsafe fn memcpy<T>(to: *mut T, from: *const T) {
    let to = to.cast::<MaybeUninit<u8>>();
    let from = from.cast::<MaybeUninit<u8>>();
    for i in 0..std::mem::size_of::<T>() {
        let b = unsafe { from.add(i).read() };
        unsafe { to.add(i).write(b) };
    }
}

fn main() {
    let mut buf = [0u8; 16];
    let src = Slot { ptr: buf.as_mut_ptr() };
    let mut dst = Slot { ptr: std::ptr::null_mut() };

    unsafe { memcpy(&mut dst, &src) };

    let p = dst.ptr;
    let q = dst.ptr;
    let r: &mut u8 = unsafe { &mut *p };
    let s: &mut u8 = unsafe { &mut *q };

    // Miri: the memcpy preserves pointer provenance, so `r` and `s` are
    // sibling mutable borrows in the same tree and the write through `s`
    // invalidates `r`.
    // Without shadow provenance surviving the bytewise copy, both field loads
    // detach from that tree and the final read through `r` succeeds silently.
    unsafe { std::ptr::write_volatile(s, 1) };
    let _val = read_ref(r);
    println!("val={_val}");
}
