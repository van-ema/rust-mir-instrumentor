# stack_slot_reuse_vec_len

Safe-code stress case for stack-slot reuse/coalescing around `mem::take` and `Vec::len`.

Pattern:
- create a short-lived pointer-sized local (`*mut u8`)
- create a larger stack object with `Vec` metadata
- call `mem::take(&mut header.ext)` (hits `core::mem::replace/read_via_copy`)
- read `header.ext.len()`

If runtime stack metadata is stale/coarse, this shape can produce false OOB reports in optimized builds.
