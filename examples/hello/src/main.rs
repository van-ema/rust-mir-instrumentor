use runtime as _;

use lib_hello;

fn main() {
    let x = 42;
    let r = &x;
    println!("r = {}", r);
    lib_hello::say_hello();
}
