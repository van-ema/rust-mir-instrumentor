// Purpose: import a Miri provenance-known-gap around partial pointer-byte
// loads. This is useful as an explicit "not yet covered" regression.
//
// Adapted from Miri:
// `tests/fail/reading_half_a_pointer.rs`
//
// Miri rejects this because the load starts one byte before the actual pointer
// bytes, so the reconstructed pointer has no valid provenance. Rusteze does
// not currently model partial-pointer-byte provenance at this granularity.
//
// Expected in --release: ok

#![allow(dead_code)]

#[repr(C, packed)]
struct Data {
    pad: u8,
    ptr: &'static i32,
}

struct Wrapper {
    align: u64,
    data: Data,
}

static G: i32 = 0;

fn main() {
    let mut w = Wrapper {
        align: 0,
        data: Data { pad: 0, ptr: &G },
    };

    let d_alias = &mut w.data as *mut _ as *mut *const u8;
    unsafe {
        let x = *d_alias;
        let _ = *x;
    }
}
