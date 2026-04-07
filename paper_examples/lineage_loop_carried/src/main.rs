// Purpose: demonstrate loop-carried reuse of a ref-backed anchor. The first
// iteration stores a reference whose semantic parent is the raw expression
// `v.as_mut_ptr().add(4)`. The second iteration rebuilds the same expression
// and must reuse the carried ancestor instead of falling back to a fresh root.

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
    let mut first: Option<&mut u8> = None;

    loop {
        let cur: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };
        if let Some(saved) = first.take() {
            let second: &mut u8 = unsafe { &mut *v.as_mut_ptr().add(4) };
            write_through(second);
            let val = read_through(saved);
            println!("{val}");
            break;
        }
        first = Some(cur);
    }
}
