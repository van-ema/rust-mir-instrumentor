#![feature(rustc_private)]
#![feature(rustc_attrs)]

#[unsafe(no_mangle)]
#[rustc_diagnostic_item = "record_ref_creation"]
pub extern "C" fn __record_ref_creation(ptr: *const u8) {
    println!("[Runtime] Reference created at address: {:?}", ptr);
}
