// Purpose: demonstrate helper-return lineage through nested aggregate
// forwarding. The pointer is returned in a nested tuple and later projected
// through `.0.1`, so the normalized anchor has to survive helper return and
// projection-heavy forwarding.

#[inline(never)]
fn nest(p: *mut u8) -> ((usize, *mut u8), u8) {
    ((0, p), 0)
}

#[inline(never)]
fn write_through(x: &mut u8) {
    unsafe { std::ptr::write_volatile(x, 1) }
}

#[inline(never)]
fn read_through(x: &mut u8) -> u8 {
    *x
}

fn main() {
    let mut v = vec![0u8; 16];
    let first: &mut u8 = unsafe { &mut *nest(v.as_mut_ptr().add(4)).0.1 };
    let second: &mut u8 = unsafe { &mut *nest(v.as_mut_ptr().add(4)).0.1 };
    write_through(second);
    let val = read_through(first);
    println!("{val}");
}
