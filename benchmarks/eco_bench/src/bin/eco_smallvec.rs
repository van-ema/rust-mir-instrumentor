use smallvec::SmallVec;
use std::hint::black_box;

fn main() {
    let iters = eco_bench::parse_iters(2_000);
    let mut acc = 0usize;

    for i in 0..iters {
        let mut v: SmallVec<[u32; 16]> = SmallVec::new();
        for j in 0..32u32 {
            v.push(j.wrapping_add(i as u32));
        }
        v.insert(3, i as u32);
        let _ = v.remove(5);
        v.truncate(20);
        let s: u32 = v.iter().copied().sum();
        acc ^= s as usize;
    }

    black_box(acc);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{acc}");
}
