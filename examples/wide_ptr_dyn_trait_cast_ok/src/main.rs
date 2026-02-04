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
    let foo = Foo(123);
    let obj: &dyn T = &foo;

    // Wide pointer: `*const dyn T` carries a vtable pointer as metadata.
    let fat: *const dyn T = obj;

    // "Thin pointer extraction": cast the wide pointer to a thin raw pointer.
    // Rust defines this cast to keep only the data address and drop the metadata (vtable).
    let thin: *const () = fat as *const ();

    // Prove the data pointer is usable while `foo` is alive.
    let p: *const u8 = thin as *const u8;
    unsafe {
        black_box(*p);
    }

    black_box(obj.id());
}

