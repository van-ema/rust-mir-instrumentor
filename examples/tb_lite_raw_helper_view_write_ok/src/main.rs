use std::hint::black_box;
use std::mem::MaybeUninit;

fn main() {
    let mut cell = MaybeUninit::<u8>::uninit();

    let base = &mut cell as *mut MaybeUninit<u8>;
    let helper = base as *const MaybeUninit<u8>;
    let writable = helper as *mut MaybeUninit<u8>;
    let reborrowed = unsafe { &mut *writable };

    reborrowed.write(117);
    let value = unsafe { cell.assume_init() };
    black_box(value);
}
