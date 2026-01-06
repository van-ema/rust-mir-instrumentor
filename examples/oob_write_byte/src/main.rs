// Purpose: write one byte past the end of a Vec allocation.
// Expected: out-of-bounds report from the runtime.

fn main() {
    let mut v = vec![0u8; 4];
    let p = unsafe { v.as_mut_ptr().add(3) }; // points to last valid element
    unsafe {
        *p.add(1) = 0xAA; // OOB by 1 byte
    }
    println!("done");
}
