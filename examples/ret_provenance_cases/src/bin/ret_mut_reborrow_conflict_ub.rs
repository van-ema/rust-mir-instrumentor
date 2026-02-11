// Case: callee reborrows from raw and returns &mut; caller still uses old raw.
// Goal: stress returned unique-reference lineage against previously created raw aliases.

#[inline(never)]
unsafe fn ret_mut_from_raw<'a>(p: *mut u8) -> &'a mut u8 {
    &mut *p
}

fn main() {
    let mut x = 0u8;
    let raw = &mut x as *mut u8;
    let r = unsafe { ret_mut_from_raw(raw) };

    unsafe {
        *raw = 1;
    }

    let _ = *r;
}
