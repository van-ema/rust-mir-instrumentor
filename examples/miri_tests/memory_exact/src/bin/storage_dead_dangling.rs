// Ported from miri/tests/fail/dangling_pointers/storage_dead_dangling.rs.
//@compile-flags: -Zmiri-disable-validation -Zmiri-permissive-provenance

static mut LEAK: usize = 0;

fn fill(v: &mut i32) {
    unsafe {
        LEAK = v as *mut _ as usize;
    }
}

fn evil() {
    let _ref = unsafe { &mut *(LEAK as *mut i32) };
}

fn main() {
    let _y;
    {
        let mut x = 0i32;
        fill(&mut x);
        _y = x;
    }
    evil();
}
