use serde_json::Value;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    if let Ok(v) = serde_json::from_slice::<Value>(&data) {
        // Round-trip to exercise serializer paths too.
        let _ = serde_json::to_vec(&v);
    }
}
