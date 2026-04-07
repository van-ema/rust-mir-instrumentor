// Purpose: demonstrate the mixed memory + SSA case. The first reference reloads
// a pointer through a memory-backed wrapper field, while the second rebuilds the
// same semantic expression through a helper return. Pointer shadow and SSA
// anchors must cooperate to preserve one shared ancestor.

#[repr(C)]
struct Wrap {
    p: *mut u8,
}

#[inline(never)]
fn id_ptr(p: *mut u8) -> *mut u8 {
    p
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
    let w = Wrap { p: unsafe { id_ptr(v.as_mut_ptr().add(4)) } };
    let first: &mut u8 = unsafe { &mut *w.p };
    let second: &mut u8 = unsafe { &mut *id_ptr(v.as_mut_ptr().add(4)) };
    write_through(second);
    let val = read_through(first);
    println!("{val}");
}
