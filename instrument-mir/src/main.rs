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
extern crate rustc_span;

use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_hir::def_id::{DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use rustc_interface::util::rustc_path;
use rustc_interface::Config;
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;
use rustc_middle::ty::{self, ParamEnv, Ty};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;
use rustc_span::{source_map::Spanned, Span};

struct MyOptimizationPass;

fn find_record_fn<'tcx>(tcx: TyCtxt<'tcx>) -> Option<DefId> {
    for &cnum in tcx.crates(()) {
        if tcx.crate_name(cnum).as_str() == "runtime" {
            let root = DefId {
                krate: cnum,
                index: CRATE_DEF_INDEX,
            };
            for child in tcx.module_children(root) {
                if let Some(name) = tcx.opt_item_name(child.res.def_id()) {
                    if name.as_str() == "_record_ref_creation" {
                        return Some(child.res.def_id());
                    }
                }
            }
        }
    }
    None
}

impl MyOptimizationPass {
    fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        println!("Running MyOptimizationPass on {:?}", body.source.def_id());

        // Make sure rustc_span is linked
        // Resolve the instrumentation function by scanning the loaded `runtime` crate for a
        // top-level symbol named `__record_ref_creation`.
        let record_fn_def_id = find_record_fn(tcx).unwrap_or_else(|| {
            eprintln!("[instrument-mir] Visible extern crates:");
            for &c in tcx.crates(()) {
                eprintln!("  - {}", tcx.crate_name(c));
            }
            panic!(
                "Failed to find `_record_ref_creation` in loaded crates. \nMake sure you pass: --extern runtime=target/release/libruntime.rlib and the function is at crate root."
            );
        });

        for (bb, block_data) in body
            .basic_blocks
            .as_mut_preserves_cfg()
            .iter_enumerated_mut()
        {
            let mut new_stmts = Vec::new();

            for stmt in block_data.statements.iter() {
                new_stmts.push(stmt.clone());

                if let StatementKind::Assign(box (_, Rvalue::Ref(_, _, place))) = &stmt.kind {
                    println!("  Inserting call to record_ref_creation for {:?}", place);

                    // Create operand for the function argument
                    let arg = Spanned {
                        node: Operand::Copy(*place),
                        span: stmt.source_info.span,
                    };

                    // Create the function operand
                    let func_operand = Operand::function_handle(
                        tcx,
                        record_fn_def_id,
                        std::iter::empty(),
                        stmt.source_info.span,
                    );

                    // Create a temporary for return value (unit)
                    let tmp_local = body
                        .local_decls
                        .push(LocalDecl::new(tcx.types.unit, stmt.source_info.span));

                    // Create a Call terminator
                    let terminator = Terminator {
                        source_info: stmt.source_info,
                        kind: TerminatorKind::Call {
                            func: func_operand,
                            args: vec![arg].into_boxed_slice(),
                            destination: Place::from(tmp_local),
                            target: Some(bb),
                            unwind: UnwindAction::Continue,
                            call_source: CallSource::Misc,
                            fn_span: stmt.source_info.span,
                        },
                    };

                    // Create a small new block (not strictly necessary yet)
                    let new_block = BasicBlockData::new(Some(terminator), false);

                    // For now, just insert a no-op statement to keep MIR consistent
                    new_stmts.push(Statement::new(
                        stmt.source_info,
                        StatementKind::FakeRead(Box::new((
                            FakeReadCause::ForLet(None),
                            Place::from(tmp_local),
                        ))),
                    ));
                }
            }

            block_data.statements = new_stmts;
        }
    }
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        let optimization_pass = MyOptimizationPass;
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
