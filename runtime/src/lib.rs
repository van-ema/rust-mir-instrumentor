// record_runtime/src/lib.rs
#![feature(rustc_private)]

#[no_mangle]
#[rustc_diagnostic_item = "record_ref_creation"]
pub extern "C" fn __record_ref_creation(ptr: *const u8) {
    println!("[Instrumentation] Reference created at: {:?}", ptr);
}
