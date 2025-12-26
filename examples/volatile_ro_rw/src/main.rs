use std::ptr;

fn main() {
    let mut x: u64 = 0x1111_2222_3333_4444;
    let pm: *mut u64 = &mut x;
    let pc: *const u64 = pm as *const u64;

    unsafe {
        // WRITE via *mut
        ptr::write_volatile(pm, 0xaaaa_bbbb_cccc_dddd);

        // READ via *const
        let y = ptr::read_volatile(pc);

        println!("x=0x{:x} y=0x{:x}", x, y);
    }
}