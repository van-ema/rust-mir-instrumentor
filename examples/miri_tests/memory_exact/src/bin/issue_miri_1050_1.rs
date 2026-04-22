// Ported from miri/tests/fail/both_borrows/issue-miri-1050-1.rs.
fn main() {
    unsafe {
        let ptr = Box::into_raw(Box::new(0u16));
        drop(Box::from_raw(ptr as *mut u32));
    }
}
