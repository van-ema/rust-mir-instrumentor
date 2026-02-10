use base64::engine::general_purpose::STANDARD;
use base64::Engine;

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    if let Ok(s) = std::str::from_utf8(&data) {
        if let Ok(decoded) = STANDARD.decode(s) {
            let _ = STANDARD.encode(&decoded);
        } else {
            let _ = STANDARD.encode(&data);
        }
    } else {
        let _ = STANDARD.encode(&data);
    }
}
