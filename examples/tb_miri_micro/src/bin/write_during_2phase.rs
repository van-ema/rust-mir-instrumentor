// Inspired by miri/tests/fail/tree_borrows/write-during-2phase.rs.
// Miri TB flags this while SB accepts it.
struct Foo(u64);

impl Foo {
    fn add(&mut self, n: u64) -> u64 {
        self.0 + n
    }
}

fn main() {
    let mut f = Foo(0);
    let alias = &mut f.0 as *mut u64;
    let _res = f.add(unsafe {
        *alias = 42;
        0
    });
}
