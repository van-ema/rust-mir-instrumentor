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

    let mut runtime_path: Option<String> = None;

    args.retain(|arg| {
        if let Some(v) = arg.strip_prefix("--runtime-path=") {
            runtime_path = Some(v.to_string());
            false
        } else {
            true
        }
    });

    // Pass custom driver flags through RUSTFLAGS so Cargo forwards them to RUSTC=instrument-mir.
    let mut extra_rf: Vec<String> = Vec::new();

    if let Some(path) = mir_out {
        extra_rf.push(format!("--mir-out={}", path));
    }
    if let Some(path) = runtime_path {
        extra_rf.push(format!("--runtime-path={}", path));
    }

    if !extra_rf.is_empty() {
        let rf = extra_rf.join(" ");
        let existing = env::var("RUSTFLAGS").unwrap_or_default();
        let new_rf = if existing.is_empty() {
            rf
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
