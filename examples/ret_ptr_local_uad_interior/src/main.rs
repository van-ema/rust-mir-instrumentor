// Purpose: return an interior pointer into a stack local and deref it in the caller.
// Expected: use-after-dead / stale epoch mismatch (panic with RUSTEZE_FAILFAST=1).
// Validates: range-based allocation lookup catches interior pointers.

fn leak_mid() -> *const i32 {
    let arr = [10i32, 20, 30, 40];
    unsafe { arr.as_ptr().add(2) }
} // arr dead here

fn main() {
    let p = leak_mid();
    let v = unsafe { *p }; // should report use-after-dead / epoch mismatch
    println!("v={v}");
}
