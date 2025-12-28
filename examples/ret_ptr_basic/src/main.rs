#[inline(never)]
fn callee(x: &i32) -> *const i32 {
    // create a raw pointer inside callee (should create tag, parent=tag(x))
    let p = x as *const i32;
    p
}

#[inline(never)]
fn sink(p: *const i32) -> i32 {
    unsafe { *p } // read in caller side
}

fn main() {
    let x = 10;
    let p = callee(&x);
    let v = sink(p);
    println!("v={}", v);
}
