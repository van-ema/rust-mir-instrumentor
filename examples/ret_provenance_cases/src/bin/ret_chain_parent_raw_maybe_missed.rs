// Case: pointer return propagates across multiple functions.
// Goal: stress provenance through f -> g -> h return chains.

#[inline(never)]
unsafe fn id1(p: *mut u8) -> *mut u8 {
    p
}

#[inline(never)]
unsafe fn id2(p: *mut u8) -> *mut u8 {
    id1(p)
}

#[inline(never)]
unsafe fn id3(p: *mut u8) -> *mut u8 {
    id2(p)
}

fn main() {
    let mut arr = [0u8; 2];
    let base = arr.as_mut_ptr();
    let ret = unsafe { id3(base) };

    unsafe {
        *ret = 10;
        *base.add(1) = 20;
    }

    std::hint::black_box(arr);
}
