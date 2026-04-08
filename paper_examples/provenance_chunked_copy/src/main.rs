// Purpose: import Miri's chunked provenance-preserving memcpy case as a
// standalone regression.
//
// Adapted from Miri:
// `tests/pass/provenance.rs` (`bytewise_custom_memcpy_chunked`)
//
// Pointers are written at misaligned offsets, copied through a chunked
// `MaybeUninit<usize>` memcpy, and then reloaded unaligned. The copied
// pointers should still dereference successfully afterward.
//
// Expected in --release: ok

use std::mem;

const PTR_SIZE: usize = mem::size_of::<&i32>();

unsafe fn memcpy<T>(to: *mut T, from: *const T) {
    assert!(mem::size_of::<T>() % mem::size_of::<usize>() == 0);
    let count = mem::size_of::<T>() / mem::size_of::<usize>();
    let to = to.cast::<mem::MaybeUninit<usize>>();
    let from = from.cast::<mem::MaybeUninit<usize>>();
    for i in 0..count {
        let b = unsafe { from.add(i).read() };
        unsafe { to.add(i).write(b) };
    }
}

fn main() {
    let mut data = [0usize; 2 * PTR_SIZE];
    let mut offsets = vec![];
    for i in 0..mem::size_of::<usize>() {
        let base = i * 2 * PTR_SIZE;
        let offset = base + i;
        offsets.push(offset);
        unsafe { data.as_mut_ptr().byte_add(offset).cast::<&i32>().write_unaligned(&42) };
    }

    let mut data2 = [0usize; 2 * PTR_SIZE];
    unsafe { memcpy(&mut data2, &data) };

    for &offset in &offsets {
        let ptr = unsafe { data2.as_ptr().byte_add(offset).cast::<&i32>().read_unaligned() };
        assert_eq!(*ptr, 42);
    }
}
