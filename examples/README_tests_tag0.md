# Tag=0 stress tests

These tiny crates are intended to trigger cases where pointer tag propagation can be missed
(tag=0 / "untagged ptr").

- `tag0_call_arg_wrapping_offset`: passes `p.wrapping_offset(0)` as an rvalue call argument.
- `tag0_call_arg_offset_add_sub`: passes `p.add(0)`, `p.sub(0)`, and `p.offset(0)` as rvalue args.
- `tag0_cast_then_call`: casts `*const u32` to `*const u8` and calls into a reader.
- `tag0_copy_chain_then_call`: copies pointer locals through a chain, then calls.
- `tag0_return_raw_ptr_from_arg`: returns a raw pointer derived from an argument (return-tag gap).
- `tag0_return_ref_created_in_callee`: creates a reference in callee and returns it as a raw ptr.
