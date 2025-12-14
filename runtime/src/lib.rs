#![feature(rustc_attrs)]
// runtime/src/lib.rs
#![allow(unused)]
#![allow(internal_features)]

#[macro_export]
macro_rules! force_runtime {
    ($sym:path) => {
        #[used]
        static _FORCE_RUNTIME: fn(usize) = $sym;
    };
}

#[no_mangle]
#[rustc_diagnostic_item = "mir_runtime_record_ref_creation"]
pub extern "C" fn __record_ref_creation(addr: usize) {
    println!("__record_ref_creation called for address: 0x{:x}", addr);
}
