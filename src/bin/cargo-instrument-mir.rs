use std::env;
use std::process::{exit, Command};

fn main() {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);

    // Path to this binary, renamed to point to the driver executable
    let driver = env::current_exe()
        .unwrap()
        .with_file_name("instrument-mir");

    // Forward all arguments after "cargo instrument-mir ..."
    let args: Vec<String> = env::args().skip(1).collect();

    let status = cmd
        .args(&args)
        .env("RUSTC_WORKSPACE_WRAPPER", &driver)
        .status()
        .expect("failed to run cargo");

    match status.code() {
        Some(0) => {}
        Some(code) => exit(code),
        None => {
            eprintln!("cargo-instrument-mir terminated by signal");
            exit(1);
        }
    }
}
