// Ported from miri/tests/fail/dangling_pointers/null_pointer_deref.rs.
#[allow(deref_nullptr)]
fn main() {
    let x: i32 = unsafe { *std::ptr::null() };
    panic!("this should never print: {}", x);
}
