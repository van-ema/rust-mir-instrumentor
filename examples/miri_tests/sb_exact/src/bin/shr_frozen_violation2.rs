// Ported from miri/tests/fail/both_borrows/shr_frozen_violation2.rs.
fn main() {
    unsafe {
        let mut x = 0;
        let ptr = std::ptr::addr_of_mut!(x);
        let frozen = &*ptr;
        let _val = *frozen;
        x = 1;
        let _val = *frozen;
        let _val = x;
        let _ = _val;
    }
}
