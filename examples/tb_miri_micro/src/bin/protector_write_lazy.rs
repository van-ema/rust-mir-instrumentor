// Inspired by miri/tests/fail/tree_borrows/protector-write-lazy.rs.
fn the_other_function(ref_to_fst_elem: &mut i32, ptr_to_vec: *mut i32) -> *mut i32 {
    *ref_to_fst_elem = 0;
    *ref_to_fst_elem = 42;
    let funky_ptr_lazy_on_fst_elem =
        unsafe { (&mut *(ptr_to_vec.wrapping_add(1))) as *mut i32 }.wrapping_sub(1);
    funky_ptr_lazy_on_fst_elem
}

fn main() {
    let mut v = vec![0, 1];
    let ptr_to_vec = v.as_mut_ptr();
    let ref_to_fst_elem = unsafe { &mut *ptr_to_vec };
    let funky_ptr = the_other_function(ref_to_fst_elem, ptr_to_vec);
    let _val = unsafe { *funky_ptr };
}
