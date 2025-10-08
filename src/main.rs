#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_middle::mir::*;

fn main() {
    println!("Rustc internal test");
}
