use criterion::{black_box, criterion_group, criterion_main, Criterion};
use smallvec::SmallVec;

fn smallvec_smoke(c: &mut Criterion) {
    c.bench_function("smallvec/smoke", |b| {
        b.iter(|| {
            let mut v: SmallVec<u8, 8> = SmallVec::new();
            for i in 0..16u8 {
                v.push(i);
            }

            let sum: u32 = v.iter().map(|&x| x as u32).sum();
            v.remove(0);
            v.insert(1, 42);

            let slice = &v[..];
            let mut v2: SmallVec<u8, 4> = SmallVec::from_slice_copy(slice);
            v2.extend_from_slice(&[1, 2, 3, 4]);

            black_box(sum + v2.len() as u32);
        })
    });
}

criterion_group!(benches, smallvec_smoke);
criterion_main!(benches);

