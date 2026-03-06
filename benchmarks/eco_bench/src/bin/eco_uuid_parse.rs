use std::hint::black_box;
use uuid::Uuid;

fn main() {
    let iters = eco_bench::parse_iters(800);
    let inputs = [
        "f9168c5e-ceb2-4faa-b6bf-329bf39fa1e4",
        "67e55044-10b1-426f-9247-bb680e5fe0c8",
        "550e8400-e29b-41d4-a716-446655440000",
        "123e4567-e89b-12d3-a456-426614174000",
    ];
    let mut fold = 0u128;

    for i in 0..iters {
        let s = inputs[(i as usize) % inputs.len()];
        let id = Uuid::parse_str(s).expect("invalid uuid literal");
        fold ^= id.as_u128();
    }

    black_box(fold);
    eco_bench::maybe_dump_rusteze_hook_profile();
    println!("{fold}");
}
