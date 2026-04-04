// Purpose: demonstrate why pointer-shadow metadata must survive bytewise copies
// of pointer-carrying objects.
//
// This is inspired by Miri's provenance tests in `tests/pass/provenance.rs`,
// especially the bytewise memcpy cases. Miri preserves the provenance of a
// pointer value when it is copied through memory byte-by-byte. Rusteze does
// not currently carry provenance through such memory-resident pointer copies.
//
// Here the raw pointer is first stored inside `Slot`, then the entire `Slot`
// object is copied bytewise into `dst`. The later field loads `dst.ptr` should
// recover the same pointer provenance as `src.ptr`, but today they do not.
// Without pointer-shadow memory, the reloaded pointers lose their common
// ancestor and TB-Lite misses the sibling invalidation.
//
// Expected in --release: ok (TB-Lite false negative — provenance lost through
// bytewise memory copy of a pointer-carrying object)

use std::mem::MaybeUninit;

#[derive(Copy, Clone)]
struct Slot {
    ptr: *mut u8,
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
    // siblings in the same tree and `s` invalidates `r`.
    // Rusteze today: the pointer is copied through memory without shadow
    // provenance, so both field loads become detached/root-like and the read
    // through `r` succeeds silently.
    let _val = *r;
    let _ = s;
    println!("val={_val}");
}
