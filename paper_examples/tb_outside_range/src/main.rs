// Purpose: import Miri's Tree Borrows outside-range protector case.
//
// Adapted from Miri:
// `tests/fail/tree_borrows/outside-range.rs`
//
// The key behavior is that a protector should only matter once the
// corresponding out-of-range location has been observed through the protected
// pointer lineage.
//
// Expected in --release: ok
//
// Current classification: known gap. Miri rejects the final write because the
// protected lineage has already observed that out-of-range location; rusteze
// does not currently model this protector condition precisely enough.

fn main() {
    unsafe {
        let data = &mut [0u8, 1, 2, 3];
        let raw = data.as_mut_ptr();
        stuff(&mut *raw, raw);
    }
}

unsafe fn stuff(x: &mut u8, y: *mut u8) {
    let xraw = x as *mut u8;
    unsafe {
        *y.add(1) = 42;
        let _ = *xraw.add(2);
        let _ = *xraw.add(3);
        *y.add(3) = 42;
    }
}
