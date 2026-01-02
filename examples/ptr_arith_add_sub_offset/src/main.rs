// Purpose: apply pointer arithmetic (add/sub/offset) and deref results.
// Expected: derived pointers get fresh tags and READ uses the derived tag.
// Validates: PtrDerive retagging and correct parent linkage (Tree Borrows style).

fn main() {
    let arr = [1i32, 2, 3, 4];
    let base = arr.as_ptr(); // from &[i32] unsize -> as_ptr backtrack path
    unsafe {
        let p1 = base.add(1); // fresh tag
        let p2 = p1.sub(1); // fresh tag again
        let p3 = base.offset(2); // fresh tag
        println!("{} {} {}", *p1, *p2, *p3);
    }
}
