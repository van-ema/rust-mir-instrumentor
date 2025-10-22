// Link dynamically to our runtime library
#[link(name = "runtime")]
unsafe extern "C" {
    unsafe fn __record_ref_creation();
}

fn main() {
    println!("Calling __record_ref_creation from runtime...");
    unsafe {
        __record_ref_creation();
    }
    println!("Done!");
}
