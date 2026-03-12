#![no_std]

extern "C" {
    fn __record_ref_creation(
        pointee_addr: usize,
        is_mut: u8,
        parent_tag: u64,
        alias_exempt: u8,
        bounds_len: usize,
    ) -> u64;
    fn __record_raw_ptr_creation(
        pointee_addr: usize,
        is_mut: u8,
        derived_from: u64,
        alias_exempt: u8,
        bounds_len: usize,
    ) -> u64;
    fn __rz_record_alloc(base_addr: usize, size: usize, live: u8);
    fn __rz_ptr_write(tag: u64, addr: usize, size: usize);
    fn __rz_ptr_write_allow_untagged(tag: u64, addr: usize, size: usize);
    fn __rz_ptr_read(tag: u64, addr: usize, size: usize);
    fn __rz_ptr_read_allow_untagged(tag: u64, addr: usize, size: usize);
    fn __rz_ptr_use(tag: u64, addr: usize);
    fn __rz_push_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize, tag: u64);
    fn __rz_take_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize) -> u64;
    fn __rz_push_ret_tag(callee_id: u64, addr: usize, tag: u64);
    fn __rz_take_ret_tag_or_root(
        callee_id: u64,
        addr: usize,
        is_mut: u8,
        alias_exempt: u8,
        bounds_len: usize,
    ) -> u64;
    fn __rz_exit_fn(callee_id: u64);
}

#[no_mangle]
pub extern "C" fn rz_hook_record_ref_creation(
    pointee_addr: usize,
    is_mut: u8,
    parent_tag: u64,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    unsafe { __record_ref_creation(pointee_addr, is_mut, parent_tag, alias_exempt, bounds_len) }
}

#[no_mangle]
pub extern "C" fn rz_hook_record_raw_ptr_creation(
    pointee_addr: usize,
    is_mut: u8,
    derived_from: u64,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    unsafe { __record_raw_ptr_creation(pointee_addr, is_mut, derived_from, alias_exempt, bounds_len) }
}

#[no_mangle]
pub extern "C" fn rz_hook_record_alloc(base_addr: usize, size: usize, live: u8) {
    unsafe { __rz_record_alloc(base_addr, size, live) }
}

#[no_mangle]
pub extern "C" fn rz_hook_ptr_write(tag: u64, addr: usize, size: usize) {
    unsafe { __rz_ptr_write(tag, addr, size) }
}

#[no_mangle]
pub extern "C" fn rz_hook_ptr_write_allow_untagged(tag: u64, addr: usize, size: usize) {
    unsafe { __rz_ptr_write_allow_untagged(tag, addr, size) }
}

#[no_mangle]
pub extern "C" fn rz_hook_ptr_read(tag: u64, addr: usize, size: usize) {
    unsafe { __rz_ptr_read(tag, addr, size) }
}

#[no_mangle]
pub extern "C" fn rz_hook_ptr_read_allow_untagged(tag: u64, addr: usize, size: usize) {
    unsafe { __rz_ptr_read_allow_untagged(tag, addr, size) }
}

#[no_mangle]
pub extern "C" fn rz_hook_ptr_use(tag: u64, addr: usize) {
    unsafe { __rz_ptr_use(tag, addr) }
}

#[no_mangle]
pub extern "C" fn rz_hook_push_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize, tag: u64) {
    unsafe { __rz_push_call_arg_tag(callee_id, arg_index, addr, tag) }
}

#[no_mangle]
pub extern "C" fn rz_hook_take_call_arg_tag(callee_id: u64, arg_index: u64, addr: usize) -> u64 {
    unsafe { __rz_take_call_arg_tag(callee_id, arg_index, addr) }
}

#[no_mangle]
pub extern "C" fn rz_hook_push_ret_tag(callee_id: u64, addr: usize, tag: u64) {
    unsafe { __rz_push_ret_tag(callee_id, addr, tag) }
}

#[no_mangle]
pub extern "C" fn rz_hook_take_ret_tag_or_root(
    callee_id: u64,
    addr: usize,
    is_mut: u8,
    alias_exempt: u8,
    bounds_len: usize,
) -> u64 {
    unsafe { __rz_take_ret_tag_or_root(callee_id, addr, is_mut, alias_exempt, bounds_len) }
}

#[no_mangle]
pub extern "C" fn rz_hook_exit_fn(callee_id: u64) {
    unsafe { __rz_exit_fn(callee_id) }
}
