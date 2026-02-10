// Inspired by miri/tests/fail/tree_borrows/strongly-protected.rs.
// Deallocating through a raw pointer while an argument `&mut` protector is active
// should be rejected by Tree Borrows.
fn inner(x: &mut i32, f: fn(*mut i32)) {
    f(x as *mut i32);
}

fn main() {
    inner(Box::leak(Box::new(0)), |raw| {
        unsafe {
            drop(Box::from_raw(raw));
        }
    });
}
