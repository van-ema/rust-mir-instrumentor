#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]

// #[cfg(not(target_os = "invalid_os"))]
// pub fn __force_link_runtime() {}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(addr: u64) {
    println!("__record_ref_creation called for address: 0x{:x}", addr);
}

#[no_mangle]
pub(crate) fn __record_ref_creation2(addr: u64) {
    println!("__record_ref_creation called for address: 0x{:x}", addr);
}
