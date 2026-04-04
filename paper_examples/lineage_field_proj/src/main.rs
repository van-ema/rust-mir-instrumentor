// Purpose: demonstrate lineage loss through a raw pointer field projection.
//
// A raw pointer is first stored in a heap-resident wrapper object, then loaded
// back twice from the same field. The two loads should recover the provenance
// of the pointer stored in `w.ptr`, so the later write through one loaded raw
// pointer invalidates the earlier mutable reference created from the other.
//
// Miri preserves that lineage because pointer provenance survives through the
// field store/load. Rusteze still loses it in this pattern under `--release`:
// the field-loaded raw pointers become detached from the original tree, so the
// final read through `r` succeeds silently.
//
// Expected in --release: ok (TB-Lite false negative — lineage lost through
// field-loaded raw-pointer provenance)

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

    // Miri: `q` and `r` share the buffer provenance, so the write through `q`
    // invalidates the earlier mutable borrow `r`.
    // Rusteze today: the field loads still detach that provenance in release,
    // so the final read through `r` is a false negative.
    let _val = *r;
    println!("val={_val}");
}
