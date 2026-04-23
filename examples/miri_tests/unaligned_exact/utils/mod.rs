#![allow(dead_code)]

#[cfg(miri)]
unsafe extern "Rust" {
    pub fn miri_promise_symbolic_alignment(ptr: *const (), align: usize);
}

#[cfg(not(miri))]
unsafe extern "C" {
    fn __rz_promise_symbolic_alignment(ptr: *const (), align: usize);
}

#[cfg(not(miri))]
#[inline]
pub unsafe fn miri_promise_symbolic_alignment(ptr: *const (), align: usize) {
    unsafe { __rz_promise_symbolic_alignment(ptr, align) }
}
