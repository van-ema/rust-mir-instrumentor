#![no_std]

extern crate core;

#[no_mangle]
pub extern "C" fn __record_ref_creation() {}

/// Force Rust metadata emission (otherwise the crate can get “flattened”)
pub fn force_linkage() -> usize {
    core::mem::size_of::<u8>()
}
