// Purpose: demonstrate lineage loss through a raw pointer field projection.
//
// A raw pointer is first stored in a heap-resident wrapper object, then loaded
// back twice from the same field. The two loads should recover the provenance
// of the pointer stored in `w.ptr`, so the later write through one loaded raw
// pointer invalidates the earlier mutable reference created from the other.
//
// Miri preserves that lineage because pointer provenance survives through the
// field store/load. Rusteze now restores the slot provenance and also recovers
// the source-level `&mut` binding that optimized MIR coalesces onto `p`, so
// the later write through `q` invalidates the earlier mutable borrow.
//
// Expected in --release: TREE_BORROWS_VIOLATION|WRITE|RawMut|1

struct Wrapper {
    ptr: *mut u8,
    _len: usize,
}

fn main() {
    let mut buf = [0u8; 16];
    let mut w = Box::new(Wrapper {
        ptr: std::ptr::null_mut(),
        _len: 16,
    });
    w.ptr = buf.as_mut_ptr();

    // Load the same raw pointer field twice from memory. These loads should
    // recover the provenance stored in `w.ptr`, not fall back to detached roots.
    let p = unsafe { std::ptr::read(std::ptr::addr_of!((*w).ptr)) };
    let q = unsafe { std::ptr::read(std::ptr::addr_of!((*w).ptr)) };

    let r: &mut u8 = unsafe { &mut *p };
    unsafe { std::ptr::write_volatile(q, 1) };

    // `q` and `r` share the recovered buffer provenance, so the write through
    // `q` invalidates the earlier mutable borrow `r`.
    let _val = *r;
    println!("val={_val}");
}
