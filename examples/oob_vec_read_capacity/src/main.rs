// OUT_OF_BOUNDS
fn main() {
    let v: Vec<u8> = Vec::with_capacity(8);
    let p = v.as_ptr();
    unsafe {
        let _x = *p.add(8); // OOB read by 1
        core::hint::black_box(_x);
    }
}
