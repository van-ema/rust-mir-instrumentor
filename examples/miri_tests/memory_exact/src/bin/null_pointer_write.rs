// Ported from miri/tests/fail/dangling_pointers/null_pointer_write.rs.

#[allow(deref_nullptr)]
fn main() {
    unsafe { *std::ptr::null_mut() = 0i32 };
}
