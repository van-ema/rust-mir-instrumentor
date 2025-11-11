use std::env;

fn main() -> Result<(), i32> {
    let cargo = env::var("CARGO").unwrap_or("cargo".into());
    let mut cmd = std::process::Command::new(cargo);
    let driver = env::current_exe().unwrap().with_file_name("instrument-mir");

    // Collect all extra arguments passed after "cargo instrument-mir"
    let mut args: Vec<String> = env::args().skip(2).collect();
    let mut mir_out: Option<String> = None;

    args.retain(|arg| {
        if let Some(v) = arg.strip_prefix("--mir-out=") {
            mir_out = Some(v.to_string());
            false
        } else {
            true
        }
    });

    if let Some(path) = mir_out {
        let rf = format!("--mir-out={}", path);
        let existing = env::var("RUSTFLAGS").unwrap_or_default();
        let new_rf = if existing.is_empty() {
            rf.clone()
        } else {
            format!("{} {}", existing, rf)
        };
        cmd.env("RUSTFLAGS", new_rf);
    }
    let status = cmd
        .arg("build")
        .env("RUSTC", driver)
        .args(&args)
        .status()
        .unwrap();
    match status.code() {
        Some(0) => Ok(()),
        Some(other) => Err(other),
        None => Err(-1),
    }
}
