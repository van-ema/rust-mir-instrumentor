// Purpose: demonstrate the harder join case where each predecessor materializes
// the same semantic pointer expression separately and the join only keeps the
// if-expression result. The later recomputation after the join must reuse the
// shared raw ancestor carried through the merged ref local.

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
    let choose_left = std::hint::black_box(true);
    let first: &mut u8 = if choose_left {
        unsafe { &mut *v.as_mut_ptr().add(4) }
    } else {
        unsafe { &mut *v.as_mut_ptr().add(4) }
    };
    let second: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };
    write_through(second);
    let val = read_through(first);
    println!("{val}");
}
