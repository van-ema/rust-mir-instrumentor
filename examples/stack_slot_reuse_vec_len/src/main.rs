struct HeaderLike {
    ext: Vec<u8>,
    pad: [u8; 64],
}

#[inline(never)]
fn exercise(flag: bool) -> usize {
    if flag {
        // Force a short-lived pointer-sized stack local before building HeaderLike.
        // In optimized builds this can encourage stack-slot reuse/coalescing.
        let mut scratch: *mut u8 = core::ptr::null_mut();
        std::hint::black_box(&mut scratch);
    }

    let mut header = HeaderLike {
        ext: vec![1, 2, 3, 4],
        pad: [7; 64],
    };

    // Mirrors the hyper path: mem::take -> core::mem::replace/read_via_copy.
    let old = std::mem::take(&mut header.ext);
    std::hint::black_box(old);
    std::hint::black_box(header.pad);

    // Safe read of Vec metadata after take.
    header.ext.len()
}

fn main() {
    let mut total = 0usize;
    // Keep runtime short in debug + full-deps instrumentation mode.
    for i in 0..2_000usize {
        total += exercise(i & 1 == 0);
    }
    std::hint::black_box(total);
}
