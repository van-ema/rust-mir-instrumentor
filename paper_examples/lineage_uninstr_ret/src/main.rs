// Purpose: demonstrate lineage loss when a pointer is returned from a function
// that the instrumentation treats as uninstrumented (RetRoot path).
//
// std::hint::black_box prevents the compiler from reasoning about the returned
// value, but more importantly: if we use a trait object (dynamic dispatch),
// the instrumentation cannot statically classify the callee and falls back to
// RetRoot — a fresh root tag with parent=0.
//
// Alternatively: a function pointer call is also unclassifiable at compile time.
// The instrumentation emits RetRoot for unclassified indirect calls.
//
// Both returned pointers get parent=0 (RetRoot). They point to the same address.
// Root-vs-root creation exception → S does not disable R at creation.
// Read-only through R → R stays Reserved → no violation (false negative).
// Miri: both carry the alloc_id of the underlying buffer → same tree → UB.
//
// Expected: ok (TB-Lite false negative — lineage lost via indirect/unclassified call)

trait PtrSource {
    fn get(&self) -> *mut u8;
}

struct Src {
    buf: *mut u8,
}

impl PtrSource for Src {
    fn get(&self) -> *mut u8 {
        self.buf
    }
}

fn get_via_dyn(src: &dyn PtrSource) -> *mut u8 {
    // Dynamic dispatch: instrumentation cannot resolve the concrete callee at
    // compile time → unclassified call → RetRoot with parent=0.
    src.get()
}

fn main() {
    let mut buf = [0u8; 8];
    let src = Src { buf: buf.as_mut_ptr() };

    // Two calls through the same trait object — both get RetRoot (parent=0).
    let p = get_via_dyn(&src);
    let q = get_via_dyn(&src);

    let r: &mut u8 = unsafe { &mut *p };
    let s: &mut u8 = unsafe { &mut *q };

    // Only read through r — no writes.
    // Miri: both share buf's alloc_id → S disables R → UB on read.
    // TB-Lite: both parent=0 → root-vs-root → R not disabled → no violation.
    let _val = *r;
    let _ = s;
    println!("val={_val}");
}
