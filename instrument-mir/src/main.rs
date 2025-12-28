// src/main.rs
#![allow(unused)]
#![feature(rustc_private)]
#![feature(box_patterns)]
extern crate rustc_abi;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_mir_transform;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_target;

mod driver;
mod instrumentation;
mod util;

use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;

use crate::driver::{set_mir_output_paths, CompilerCallbacks};
use crate::util::prefixed_path;

fn main() {
    let mut callbacks = CompilerCallbacks {};

    let handler = EarlyDiagCtxt::new(ErrorOutputType::HumanReadable {
        kind: HumanReadableErrorType::Default,
        color_config: ColorConfig::Auto,
    });
    rustc_driver::init_rustc_env_logger(&handler);
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let mut args: Vec<String> = std::env::args().collect();

        let mut mir_out: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--mir-out=") {
                mir_out = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        let mut runtime_path: Option<String> = None;

        args.retain(|arg| {
            if let Some(rest) = arg.strip_prefix("--runtime-path=") {
                runtime_path = Some(rest.to_string());
                false
            } else {
                true
            }
        });

        if let Some(p) = mir_out {
            let before = prefixed_path(&p, "before.");
            let after = prefixed_path(&p, "after.");
            set_mir_output_paths(before, after);
        }

        // Cargo probes the compiler with `-vV` (verbose version) before building.
        // That invocation won't carry our custom flags, so we must not require them.
        let is_version_probe = args
            .iter()
            .any(|a| a == "-vV" || a == "-V" || a == "--version");

        if let Some(runtime_path) = runtime_path {
            args.push("-Zunstable-options".to_string());
            args.push(format!("-L{}", runtime_path));
            args.push(format!(
                "--extern=force:runtime={}/libruntime.rlib",
                runtime_path
            ));
        } else if !is_version_probe {
            panic!("missing --runtime-path argument (pass it via `cargo instrument-mir --runtime-path=...`)");
        }
        // args.push("-Zdump-mir=main".to_string());
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}
