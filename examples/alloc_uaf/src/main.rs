use std::alloc::{alloc, dealloc, Layout};

fn main() {
    unsafe {
        let layout = Layout::from_size_align(16, 8).unwrap();
        let p = alloc(layout);
        *p = 1;

        dealloc(p, layout);

        // UAF:
        *p = 2;
    }
}
