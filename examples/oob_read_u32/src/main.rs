fn main() {
    let v = vec![1u32, 2u32, 3u32, 4u32];
    let p = unsafe { v.as_ptr().add(4) }; // one past the end (legal to compute, illegal to deref)
    unsafe {
        let _x = *p; // OOB read, size should be 4
    }
}
