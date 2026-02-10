use toml::Value;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    if let Ok(s) = std::str::from_utf8(&data) {
        if let Ok(v) = toml::from_str::<Value>(s) {
            let _ = toml::to_string(&v);
        }
    }
}
