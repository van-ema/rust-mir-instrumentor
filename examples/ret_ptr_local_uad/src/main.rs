// Purpose: return a pointer to a stack local and deref it in the caller.
// Expected: use-after-dead / stale epoch mismatch (panic with RUSTEZE_FAILFAST=1).
// Validates: return-value retagging and stack allocation epoch tracking.

fn leak_ptr() -> *const i32 {
    let x = 123i32;
    &x as *const i32
} // x dead here

fn main() {
    let p = leak_ptr();
    let v = unsafe { *p }; // should report use-after-dead / epoch mismatch
    println!("v={v}");
}
