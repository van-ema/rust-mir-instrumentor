use libloading::{Library, Symbol};
use std::path::PathBuf;

fn main() {
    let lib_path = PathBuf::from(
        "/Users/emanuelevannacci/github/rust-mir-instrumentor/target/release/liblib_hello.dylib",
    );

    // Load it at runtime
    unsafe {
        let lib = Library::new(lib_path).expect("Failed to load library");
        // Get a handle to the exported function
        let func: Symbol<unsafe extern "C" fn()> = lib.get(b"say_hello").expect("Symbol not found");
        // Call the function!
        func();
    }
}
