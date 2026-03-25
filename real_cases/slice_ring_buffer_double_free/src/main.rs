use slice_ring_buffer::SliceRingBuffer;

#[derive(Debug, Clone)]
struct StructA(String);

impl Drop for StructA {
    fn drop(&mut self) {
        println!("Dropping StructA with data at: {:?}", self.0.as_ptr());
    }
}

fn main() {
    let mut dq1 = SliceRingBuffer::new();
    dq1.push_back(StructA(String::from("AAAA")));
    println!("pushed");

    dq1.pop_back();
    println!("popped");

    let other = &[StructA(String::from("BBBB"))];
    dq1.extend_from_slice(other);
    println!("extended: {:?}", dq1);
}
