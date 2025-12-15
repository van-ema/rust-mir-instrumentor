#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize, u8) = $sym;
    };
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(addr: usize, is_mut: u8) {
    let kind = if is_mut != 0 { "mut" } else { "shared" };
    println!("__record_ref_creation called: addr=0x{:x}, kind={}", addr, kind);
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_raw_ptr_creation"]
pub extern "C" fn __record_raw_ptr_creation(addr: usize, is_mut: u8) {
    let kind = if is_mut != 0 { "mut" } else { "const" };
    println!("__record_raw_ptr_creation called: addr=0x{:x}, kind={}", addr, kind);
}
