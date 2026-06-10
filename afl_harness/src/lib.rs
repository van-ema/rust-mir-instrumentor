use std::io::Read;

pub fn read_input() -> Vec<u8> {
    let mut data = Vec::new();
    let mut args = std::env::args_os();
    let _ = args.next(); // argv0
    if let Some(path) = args.next() {
        if let Ok(bytes) = std::fs::read(path) {
            return bytes;
        }
    }
    let _ = std::io::stdin().read_to_end(&mut data);
    data
}
