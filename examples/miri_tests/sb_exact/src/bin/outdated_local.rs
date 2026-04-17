// Ported from miri/tests/fail/both_borrows/outdated_local.rs.
fn main() {
    let mut x = 0;
    let y: *const i32 = &x;
    x = 1;
    assert_eq!(unsafe { *y }, 1);
    assert_eq!(x, 1);
}
