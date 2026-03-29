use slab::Slab;

fn main() {
    let mut slab = Slab::new();
    let key1 = slab.insert(1u64);
    let key2 = slab.insert(2u64);

    let (a, b) = unsafe { slab.get2_unchecked_mut(key1, key2) };
    std::mem::swap(a, b);

    println!("{} {}", slab[key1], slab[key2]);
}
