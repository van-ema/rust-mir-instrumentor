use std::hint::black_box;
use std::mem::MaybeUninit;

fn main() {
    // This mirrors the http::header::name scratch-buffer path used by hyper:
    // a MaybeUninit-wrapped array is materialized and assumed-init as a whole.
    let arr = MaybeUninit::<[MaybeUninit<u8>; 64]>::uninit();
    let arr = unsafe { arr.assume_init() };
    black_box(arr);
}
