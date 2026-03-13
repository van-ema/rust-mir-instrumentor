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
mod unsafe_dataflow;
mod util;

use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;

use crate::driver::{set_mir_output_paths, CompilerCallbacks, NoopCallbacks};
use crate::instrumentation::debug_classify_call_effect;
use crate::util::prefixed_path;

fn find_runtime_rlib(runtime_path: &str, crate_name: &str) -> Option<String> {
    let exact = format!("{runtime_path}/lib{crate_name}.rlib");
    if std::path::Path::new(&exact).exists() {
        return Some(exact);
    }

    let prefix = format!("lib{crate_name}-");
    let entries = std::fs::read_dir(runtime_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".rlib") {
            return Some(entry.path().display().to_string());
        }
    }
    None
}

fn find_runtime_dylib(runtime_path: &str, crate_name: &str) -> Option<String> {
    let candidates = [
        format!("{runtime_path}/lib{crate_name}.dylib"),
        format!("{runtime_path}/lib{crate_name}.so"),
        format!("{runtime_path}/{crate_name}.dll"),
    ];
    for candidate in candidates {
        if std::path::Path::new(&candidate).exists() {
            return Some(candidate);
        }
    }
    None
}

fn rustc_arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let eq_prefix = format!("{flag}=");
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if let Some(rest) = arg.strip_prefix(&eq_prefix) {
            return Some(rest);
        }
        if arg == flag {
            return args.get(i + 1).map(|s| s.as_str());
        }
        i += 1;
    }
    None
}

fn rustc_arg_contains_value(args: &[String], flag: &str, needle: &str) -> bool {
    let eq_prefix = format!("{flag}=");
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        let value = if let Some(rest) = arg.strip_prefix(&eq_prefix) {
            Some(rest)
        } else if arg == flag {
            args.get(i + 1).map(|s| s.as_str())
        } else {
            None
        };
        if let Some(value) = value {
            if value.split(',').any(|part| part.trim() == needle) {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn rustc_arg_contains_any_value(args: &[String], flag: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| rustc_arg_contains_value(args, flag, needle))
}

fn is_std_bootstrap_crate(crate_name: Option<&str>) -> bool {
    matches!(
        crate_name,
        Some(
            "core"
                | "compiler_builtins"
                | "rustc_std_workspace_core"
                | "rustc_std_workspace_alloc"
                | "rustc_std_workspace_std"
                | "panic_abort"
                | "panic_unwind"
                | "unwind"
                | "std_detect"
        )
    )
}

fn source_path_arg(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|arg| arg.ends_with(".rs"))
        .map(|s| s.as_str())
}

fn is_rust_library_source(path: &std::path::Path) -> bool {
    let path = path.to_string_lossy();
    path.contains("/lib/rustlib/src/rust/library/") || path.contains("/rust/library/")
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map_or(default, |v| v != "0" && v.to_ascii_lowercase() != "false")
}

fn workspace_root() -> Option<std::path::PathBuf> {
    std::env::var_os("RZ_WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum StdlibMode {
    None,
    Core,
    CoreAlloc,
    All,
}

impl StdlibMode {
    fn from_env() -> Self {
        match std::env::var("RZ_INSTRUMENT_STDLIB")
            .unwrap_or_else(|_| "none".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "core" => Self::Core,
            "core_alloc" | "core+alloc" | "core,alloc" => Self::CoreAlloc,
            "1" | "true" | "all" => Self::All,
            _ => Self::None,
        }
    }

    fn includes(self, crate_name: Option<&str>) -> bool {
        match (self, crate_name) {
            (Self::None, _) => false,
            (Self::Core, Some("core")) => true,
            (Self::CoreAlloc, Some("core" | "alloc")) => true,
            (Self::All, Some("core" | "alloc" | "std")) => true,
            _ => false,
        }
    }
}

fn main() {
    if let Ok(def_path) = std::env::var("RZ_DEBUG_MATCH") {
        let effect = debug_classify_call_effect(&def_path);
        eprintln!(
            "[rusteze][trace] debug_classify_call_effect: {} => {}",
            def_path, effect
        );
        std::process::exit(0);
    }

    let handler = EarlyDiagCtxt::new(ErrorOutputType::HumanReadable {
        kind: HumanReadableErrorType::Default,
        color_config: ColorConfig::Auto,
    });
    rustc_driver::init_rustc_env_logger(&handler);
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let mut args: Vec<String> = std::env::args().collect();

        let mut mir_out: Option<String> = std::env::var("RZ_MIR_OUT").ok();

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

        // Cargo probes the compiler before building. Those invocations won't carry our
        // custom flags, so we must not require them.
        let is_query_probe = args
            .iter()
            .any(|a| a == "-vV" || a == "-V" || a == "--version" || a.starts_with("--print"));
        let crate_name = rustc_arg_value(&args, "--crate-name");
        let is_linkable_target = rustc_arg_contains_any_value(
            &args,
            "--crate-type",
            &["bin", "dylib", "cdylib", "staticlib"],
        );
        let is_build_script = matches!(crate_name, Some("build_script_build"));
        let is_proc_macro = rustc_arg_contains_value(&args, "--crate-type", "proc-macro");
        let is_runtime_crate = matches!(crate_name, Some("runtime" | "runtime_abi"));
        let is_bootstrap_std_crate = is_std_bootstrap_crate(crate_name);
        let source_path = source_path_arg(&args).map(std::path::PathBuf::from);
        let is_rust_library_crate = source_path
            .as_ref()
            .is_some_and(|src| is_rust_library_source(src));
        let is_stdlib_side_crate = matches!(crate_name, Some("core" | "alloc" | "std"))
            || is_bootstrap_std_crate
            || is_rust_library_crate;
        let workspace_root = workspace_root();
        let is_workspace_source = source_path
            .as_ref()
            .zip(workspace_root.as_ref())
            .is_some_and(|(src, cwd)| src.starts_with(cwd));
        let is_build_std_support_crate =
            if rustc_arg_contains_value(&args, "-Z", "force-unstable-if-unmarked") {
                let src = source_path
                    .as_ref()
                    .map(|p| {
                        let is_workspace = workspace_root
                            .as_ref()
                            .is_some_and(|cwd| p.starts_with(cwd));
                        !is_rust_library_source(p) && !is_workspace
                    })
                    .unwrap_or(false);
                src
            } else {
                false
            };
        let stdlib_mode = StdlibMode::from_env();
        let instrument_all_deps = env_flag("RZ_INSTRUMENT_ALL_DEPS", true);
        let skip_runtime_hooks = env_flag("RZ_SKIP_RUNTIME_HOOKS", false);
        let wants_instrumentation = is_workspace_source
            || stdlib_mode.includes(crate_name)
            || (instrument_all_deps && !is_stdlib_side_crate);
        let wants_runtime_externs = wants_instrumentation
            || (skip_runtime_hooks && stdlib_mode.includes(crate_name));
        let needs_runtime_externs = runtime_path.is_some()
            && !is_query_probe
            && !is_build_script
            && !is_proc_macro
            && !is_runtime_crate
            && !is_bootstrap_std_crate
            && !is_build_std_support_crate
            && wants_runtime_externs;
        let needs_instrumentation = needs_runtime_externs && wants_instrumentation && !skip_runtime_hooks;
        let missing_runtime_path = runtime_path.is_none()
            && !is_query_probe
            && !skip_runtime_hooks
            && wants_instrumentation;

        if !needs_runtime_externs {
            return rustc_driver::run_compiler(&args, &mut NoopCallbacks);
        }

        if let Some(p) = mir_out {
            let before = prefixed_path(&p, "before.");
            let after = prefixed_path(&p, "after.");
            for path in [&before, &after] {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent).unwrap();
                    }
                }
            }
            set_mir_output_paths(before, after);
        }

        if let Some(runtime_path) = runtime_path {
            args.push("-Zunstable-options".to_string());
            args.push(format!("-L{}", runtime_path));
            let runtime_dylib = find_runtime_dylib(&runtime_path, "runtime");
            let runtime_abi_rlib = find_runtime_rlib(&runtime_path, "runtime_abi");
            let has_runtime = runtime_dylib.is_some();
            let has_runtime_abi = runtime_abi_rlib.is_some();

            if let Some(runtime_abi_rlib) = runtime_abi_rlib {
                args.push(format!("--extern=force:runtime_abi={}", runtime_abi_rlib));
            }
            if has_runtime && !is_stdlib_side_crate && is_linkable_target {
                args.push(format!("-Lnative={}", runtime_path));
                args.push("-ldylib=runtime".to_string());
            }
            if !has_runtime_abi && (!has_runtime || is_stdlib_side_crate || !is_linkable_target) {
                panic!(
                    "missing runtime hook crates in --runtime-path={} (expected libruntime.rlib and/or libruntime_abi.rlib)",
                    runtime_path
                );
            }
        } else if missing_runtime_path {
            panic!("missing --runtime-path argument (pass it via `cargo instrument-mir --runtime-path=...`)");
        }
        // args.push("-Zdump-mir=main".to_string());
        if needs_instrumentation {
            let mut callbacks = CompilerCallbacks {};
            rustc_driver::run_compiler(&args, &mut callbacks)
        } else {
            rustc_driver::run_compiler(&args, &mut NoopCallbacks)
        }
    }))
}
