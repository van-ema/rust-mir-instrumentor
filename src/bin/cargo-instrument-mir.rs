use std::env;

fn main() -> Result<(), i32> {
    let cargo = env::var("CARGO").unwrap_or("cargo".into());
    let mut cmd = std::process::Command::new(cargo);
    let driver = env::current_exe().unwrap().with_file_name("instrument-mir");

    // Collect all extra arguments passed after "cargo unsafe-emit"
    let args: Vec<String> = env::args().skip(2).collect();

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
