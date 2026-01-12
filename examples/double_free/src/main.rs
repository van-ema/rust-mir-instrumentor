use std::alloc::{alloc, dealloc, Layout};

fn main() {
    unsafe {
        let layout = Layout::from_size_align(16, 8).unwrap();
        let p = alloc(layout);
        if p.is_null() {
            return;
        }

        dealloc(p, layout);
        dealloc(p, layout); // double free
    }
}
