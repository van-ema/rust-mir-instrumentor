use std::alloc::{alloc, dealloc, Layout};

fn main() {
    unsafe {
        let layout = Layout::from_size_align(16, 8).unwrap();
        let p = alloc(layout);
        if p.is_null() {
            return;
        }

        p.write_bytes(0x11, 16);

        dealloc(p, layout);

        // UAF
        *p = 0x22;
    }
}
