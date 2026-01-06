#[repr(C)]
struct S {
    a: u8,
    b: u64,
}

fn main() {
    let mut s = S { a: 1, b: 2 };
    let p = &mut s as *mut S;

    unsafe {
        // This is an in-bounds write:
        (*p).a = 10;

        // Now craft an interior pointer into the struct then step beyond it:
        let q = (p as *mut u8).add(std::mem::size_of::<S>() - 1);
        core::ptr::write_unaligned(q.add(8) as *mut u64, 0xDEADBEEF); // OOB by 8 bytes, no alignment check
    }
}
