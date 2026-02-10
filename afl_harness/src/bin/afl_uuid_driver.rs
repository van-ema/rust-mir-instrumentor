fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    if let Ok(s) = std::str::from_utf8(&data) {
        if let Ok(id) = uuid::Uuid::parse_str(s) {
            let _ = id.as_u128();
            let _ = id.hyphenated().to_string();
            let _ = id.simple().to_string();
        }
    }
}
