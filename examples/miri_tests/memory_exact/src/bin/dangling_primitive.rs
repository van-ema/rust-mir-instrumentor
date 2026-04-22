// Ported from miri/tests/fail/dangling_pointers/dangling_primitive.rs.
fn main() {
    let ptr = {
        let x = 0usize;
        &x as *const usize
    };
    unsafe {
        let _ = *ptr;
    }
}
