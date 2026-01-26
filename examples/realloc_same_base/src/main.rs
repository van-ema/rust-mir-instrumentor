use std::alloc::{alloc, dealloc, realloc, Layout};

fn main() {
    unsafe {
        let layout = Layout::from_size_align(64, 8).unwrap();
        let p = alloc(layout);
        if p.is_null() {
            return;
        }

        // Initialize memory so the allocation is definitely live and tracked.
        p.write_bytes(0xAA, layout.size());

        // Realloc to the same size. Many allocators keep the base address.
        let q = realloc(p, layout, layout.size());
        if q.is_null() {
            return;
        }

        // Always safe to use the new pointer.
        *q = 0xBB;

        // If realloc kept the base, the old pointer must remain valid.
        // This used to trigger USE_AFTER_DEAD with premature death recording.
        if q == p {
            *p = 0xCC;
        }

        dealloc(q, layout);
    }
}
