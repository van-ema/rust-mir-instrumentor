// runtime/src/lib.rs
#![feature(no_core)]
#![no_std]

// Prevent the function from being optimized away
#[unsafe(no_mangle)]
pub extern "C" fn record_ref_creation() {
    // Minimal no-op body
    // In a real runtime you could record or log this
}
