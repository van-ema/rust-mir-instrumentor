fn main() {
    let x = 10;
    let p = &x as *const i32;
    let v = dep::callee(p);
    println!("v={v}");
}