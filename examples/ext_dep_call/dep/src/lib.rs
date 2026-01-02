#[inline(never)]
pub fn callee(p: *const i32) -> i32 {
    unsafe { *p } // forces a deref read in the dep crate
}