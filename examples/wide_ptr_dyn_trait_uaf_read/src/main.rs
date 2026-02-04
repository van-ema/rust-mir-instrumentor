use std::hint::black_box;

trait T {
    fn id(&self) -> u64;
}

struct Foo(u64);

impl T for Foo {
    fn id(&self) -> u64 {
        self.0
    }
}

fn main() {
    let b: Box<dyn T> = Box::new(Foo(77));
    let obj: &dyn T = &*b;

    // Wide raw pointer: `*const dyn T`.
    let fat: *const dyn T = obj;

    // Extract a thin pointer from the wide pointer (drop vtable metadata).
    let thin: *const u8 = (fat as *const ()) as *const u8;

    // Free the allocation.
    drop(b);

    unsafe {
        // Use-after-free via a thin pointer derived from a wide pointer.
        black_box(*thin);
    }
}

