use std::hint::black_box;

#[inline(never)]
fn with_mut<F: FnOnce(&mut *mut ())>(slot: &mut *mut (), f: F) {
    f(slot);
}

#[inline(never)]
fn capture_raw_arg(data: &mut *mut (), ptr: *const u8, len: usize) {
    with_mut(data, |shared| {
        let shared_value = *shared;
        let ptr_slot_ref = &ptr;
        let len_slot_ref = &len;

        black_box(shared_value);
        black_box(*ptr_slot_ref);
        black_box(*len_slot_ref);
    });
}

fn main() {
    let mut backing = vec![7_u8, 8, 9];
    let mut data = backing.as_mut_ptr().cast::<()>();
    let ptr = backing.as_ptr();

    capture_raw_arg(&mut data, ptr, backing.len());
    black_box(data);
}
