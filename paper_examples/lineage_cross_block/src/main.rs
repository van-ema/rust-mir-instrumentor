// Purpose: demonstrate lineage recovery through a cross-block phi (ret_take at
// a call boundary correctly recovers the return tag).
//
// The `choose()` helper has a phi-like structure across basic blocks:
//   bb1: _0 = copy _2;  goto bb3
//   bb2: _0 = copy _3;  goto bb3
//   bb3: return
//
// Despite the phi in `choose`, the CALLER recovers the return tag via ret_take
// at the call boundary. Both `_6` and `_9` in main get the tag recovered from
// `choose`'s return value — they share the same parent (buf's tag).
//
// TB-Lite correctly detects the aliasing violation: both refs are siblings in
// the same tree, `_13`'s creation fires a foreign-write on `_12`, disabling it.
// The read through `_12` fires TB_LITE_INVALIDATED.
//
// This example shows that ret_take DOES bridge cross-block phis at call
// boundaries — the false negative only occurs for intra-function phis where
// there is no call boundary to trigger ret_take.
//
// Expected: TREE_BORROWS_VIOLATION (TB-Lite catches via ret_take recovery)

fn choose(flag: bool, a: *mut u8, b: *mut u8) -> *mut u8 {
    // MIR: phi across bb1/bb2 — _0 assigned in two different blocks.
    // Caller recovers the return tag via ret_take regardless.
    if flag { a } else { b }
}

fn main() {
    let mut buf = [0u8; 8];
    let base = buf.as_mut_ptr();

    // Both calls go to the same address — aliasing is real.
    let p = choose(true,  base, base);  // ret_take → p gets buf's tag
    let q = choose(false, base, base);  // ret_take → q gets buf's tag

    let r: &mut u8 = unsafe { &mut *p };  // R: parent = p's tag
    let s: &mut u8 = unsafe { &mut *q };  // S: parent = q's tag; fires foreign-write on R

    // R is now Disabled. Reading through R fires TB_LITE_INVALIDATED.
    let _val = *r;
    let _ = s;
    println!("val={_val}");
}
