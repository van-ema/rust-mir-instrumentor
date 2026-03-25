#![forbid(unsafe_code)]
use smallvec::SmallVec;

fn main() {
    let mut v: SmallVec<[u8; 0]> = SmallVec::new();
    v.push(123);

    let s = String::from("Hello!");
    println!("{}", s);
    let iter = (0u8..=255).filter(|n| n % 2 == 0);
    assert_eq!(iter.size_hint().0, 0);

    v.insert_many(0, iter);

    assert!(v.as_ptr_range().contains(&s.as_ptr()));
    println!("{}", s);
}
