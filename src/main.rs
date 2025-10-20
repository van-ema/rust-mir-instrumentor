// src/main.rs
#![allow(unused)]
#![feature(rustc_private)]
#![feature(box_patterns)]
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_mir_transform;
extern crate rustc_session;

use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_hir::def_id::{DefId, DefIndex, LocalDefId, LOCAL_CRATE};
use rustc_interface::util::rustc_path;
use rustc_interface::Config;
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;
use rustc_middle::ty::{self, ParamEnv, Ty};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;

struct MyOptimizationPass;

impl MyOptimizationPass {
    fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        println!("Running MyOptimizationPass");
    }
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        // Create an instance of your optimization pass
        let optimization_pass = MyOptimizationPass;
        // Run the optimization pass
        optimization_pass.run_pass(tcx, &mut body);

        tcx.arena.alloc(body)
    };

struct CompilerCallbacks;

impl rustc_driver::Callbacks for CompilerCallbacks {
    fn config(&mut self, _config: &mut Config) {
        _config.override_queries = Some(|_session, queries| {
            queries.optimized_mir = CUSTOM_OPT_MIR;
        });
    }
}

fn main() {
    let mut callbacks = CompilerCallbacks {};

    let handler = EarlyDiagCtxt::new(ErrorOutputType::HumanReadable {
        kind: HumanReadableErrorType::Default,
        color_config: ColorConfig::Auto,
    });
    rustc_driver::init_rustc_env_logger(&handler);
    std::process::exit(rustc_driver::catch_with_exit_code(move || {
        let args: Vec<String> = std::env::args().collect();
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}
