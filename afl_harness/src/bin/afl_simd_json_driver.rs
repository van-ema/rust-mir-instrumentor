fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    // simd-json parses in place; feed it mutable input and round-trip on success.
    let mut input = data.clone();
    if let Ok(value) = simd_json::to_owned_value(&mut input) {
        let _ = simd_json::to_string(&value);
    }
}
