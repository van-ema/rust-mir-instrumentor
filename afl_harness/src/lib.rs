use std::io::Read;

pub fn read_input() -> Vec<u8> {
    let mut data = Vec::new();
    let mut args = std::env::args_os();
    let _ = args.next(); // argv0
    if let Some(path) = args.next() {
        if let Ok(bytes) = std::fs::read(path) {
            // AFL repro/fuzz runs pass input via `@@`, so `std::fs::read`
            // allocates and fills the buffer inside uninstrumented `std`.
            // Materialize the bytes into a distinct harness-local `Vec`
            // before handing them to the target.
            let mut remat = Vec::with_capacity(bytes.len());
            for b in bytes {
                remat.push(b);
            }
            return remat;
        }
    }
    let _ = std::io::stdin().read_to_end(&mut data);
    data
}
