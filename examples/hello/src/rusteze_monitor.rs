// runtime/src/lib.rs
#![allow(unused)]

#[unsafe(no_mangle)]
pub extern "C" fn __record_ref_creation(addr: u64) {
    println!("__record_ref_creation called for address: 0x{:x}", addr);
}

#[unsafe(no_mangle)]
pub(crate) fn __record_ref_creation2(addr: u64) {
    println!("__record_ref_creation called for address: 0x{:x}", addr);
}
