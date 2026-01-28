#[repr(C)]
struct Vtable {
    f: fn(i32) -> i32,
}

fn plus_one(x: i32) -> i32 {
    x + 1
}

static VTABLE: Vtable = Vtable { f: plus_one };

#[repr(C)]
struct Obj {
    vtable: *const Vtable,
}

fn main() {
    let o = Obj { vtable: &VTABLE as *const Vtable };

    unsafe {
        // Reads a function pointer from a raw pointer to static metadata.
        // Without the raw-read suppression, this would report WILD_POINTER in release mode.
        let f = (*o.vtable).f;
        let out = f(41);
        println!("{}", out);
    }
}
