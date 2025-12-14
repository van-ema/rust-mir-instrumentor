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

use rustc_abi::ExternAbi;
use rustc_errors::{emitter::HumanReadableErrorType, ColorConfig};
use rustc_hir::def_id::{DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use rustc_interface::util::rustc_path;
use rustc_interface::Config;
use rustc_middle::mir::interpret::{AllocId, Scalar};
use rustc_middle::mir::*;
use rustc_middle::mir::{Const, ConstOperand, ConstValue};
use rustc_middle::ty::{self, print, ParamEnv, Ty, TyCtxt};
use rustc_session::config::ErrorOutputType;
use rustc_session::EarlyDiagCtxt;
use rustc_span::{source_map::Spanned, Span};

use rustc_hir::Safety;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::pretty::write_mir_fn;
use rustc_middle::ty::TyKind;
use rustc_span::symbol::Symbol;
use std::num::NonZeroU64;
use std::sync::Mutex;

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::sync::OnceLock;

static MIR_OUT: OnceLock<String> = OnceLock::new();

struct MyOptimizationPass;

impl MyOptimizationPass {

    // fn print_runtime_items<'tcx>(&self, tcx: TyCtxt<'tcx>) {
    //     for &cnum in tcx.crates(()).iter() {
    //         let crate_name = tcx.crate_name(cnum);
    //         if crate_name.as_str() == "runtime" {
    //             println!("Items in runtime crate:");
    //             let items = tcx.hir_crate_items(());

    //             // Use `free_items` to iterate over non-associated items
    //             for item_id in items.free_items() {
    //                 let def_id = item_id.owner_id.def_id;
    //                 if let Some(name) = tcx.opt_item_name(def_id) {
    //                     println!(" - Item: {}", name);
    //                 } else {
    //                     println!(" - Unnamed item: {:?}", def_id);
    //                 }
    //                 // Print additional debugging information about the item
    //                 let item_kind = tcx.def_kind(def_id);
    //                 println!("   - DefKind: {:?}", item_kind);

    //                 let span = tcx.def_span(def_id);
    //                 println!("   - Span: {:?}", span);
    //             }
    //         }
    //     }
    // }

    fn find_def_id_by_name<'tcx>(&self, tcx: TyCtxt<'tcx>, target_name: &str) -> Option<DefId> {
        for &cnum in tcx.crates(()).iter() {
            let crate_name = tcx.crate_name(cnum);
            if crate_name.as_str() == "runtime" {
                println!("Searching for '{}' in runtime crate:", target_name);
                let items = tcx.exported_non_generic_symbols(cnum);
                println!("Found {} items", items.len());
                for (symbol, _) in items {
                    match symbol {
                        ExportedSymbol::NonGeneric(def_id) | ExportedSymbol::Generic(def_id, _) => {
                            if let Some(name) = tcx.opt_item_name(*def_id) {
                                println!(" - Checking item: {}", name);
                                if name.as_str() == target_name {
                                    println!(" - Match found for '{}'", target_name);
                                    return Some(*def_id);
                                }
                            } else {
                                println!(" - Unnamed item: {:?}", def_id);
                            }
                        }
                        ExportedSymbol::NoDefId(symbol_name) => {
                            println!(" - Symbol without DefId: {:?}", symbol_name);
                        }
                        ExportedSymbol::DropGlue(ty) => {
                            println!(" - DropGlue for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlueCtorShim(ty) => {
                            println!(" - AsyncDropGlueCtorShim for type: {:?}", ty);
                        }
                        ExportedSymbol::AsyncDropGlue(def_id, ty) => {
                            println!(" - AsyncDropGlue for DefId: {:?}, type: {:?}", def_id, ty);
                        }
                        ExportedSymbol::ThreadLocalShim(def_id) => {
                            println!(" - ThreadLocalShim for DefId: {:?}", def_id);
                        }
                        _ => {
                            println!(" - Unhandled ExportedSymbol variant");
                        }
                    }
                }
            }
        }
        println!("No match found for '{}'", target_name);
        None
    }

    fn run_pass<'tcx>(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let def_id = body.source.def_id();
        let def_path = tcx.def_path_str(def_id);

        if def_path.contains("runtime") {
            println!("Skipping optimization for {}", def_path);
            return;
        }

        let crate_name = tcx.crate_name(def_id.krate);

        // Skip the `runtime` crate
        if crate_name.as_str() == "runtime" {
            println!(
                "Skipping optimization for item in runtime crate: {:?}",
                def_id
            );
            return;
        }

        println!(
            "Running MyOptimizationPass on {:?} {:?}",
            body.source.def_id(),
            def_path
        );

        println!("Loaded crates:");
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum);
            println!(" - {:?} (cnum: {:?})", name, cnum);
        }

        // self.print_runtime_items(tcx);

        let def_id = self
            .find_def_id_by_name(tcx, "__record_ref_creation")
            .expect("missing '__record_ref_creation' definition");
        let func_operand_base =
            move |sp: Span| Operand::function_handle(tcx, def_id, std::iter::empty(), sp);

        // We'll collect all insertion points first to avoid borrow issues.
        let mut insert_points = Vec::new();
        for (bb, block_data) in body.basic_blocks.as_mut_preserves_cfg().iter_enumerated() {
            for (stmt_idx, stmt) in block_data.statements.iter().enumerate() {
                if let StatementKind::Assign(box (place, Rvalue::Ref(_, _, _))) = &stmt.kind {
                    insert_points.push((bb, stmt_idx, stmt.source_info, place.clone()));
                    println!(
                        "Found ref creation at block {:?}, stmt idx {}: {:?}",
                        bb, stmt_idx, stmt
                    );
                }
                if let StatementKind::Assign(box (_, Rvalue::RawPtr(mutbl, src_place))) = &stmt.kind
                {
                    println!(
                        "Found raw pointer creation at block {:?}, stmt idx {}: {:?} = &raw {:?} {:?}",
                        bb, stmt_idx, stmt, mutbl, src_place
                    );
                }
            }
        }

        // Insert in reverse order to not invalidate indices
        for (bb, stmt_idx, source_info, place) in insert_points.into_iter().rev() {
            let (orig_term, is_cleanup) = {
                let bd = &mut body.basic_blocks_mut()[bb];
                let term = bd.terminator.take();
                let cleanup = bd.is_cleanup;
                (term, cleanup)
            };

            println!("Terminator at block {:?}: {:?}", bb, orig_term);

            // Build continuation block now (no outstanding borrow of `bb`)
            let cont_block = {
                let cont_data = BasicBlockData::new(orig_term, is_cleanup);
                body.basic_blocks_mut().push(cont_data)
            };

            // Build function operand and temp local (no outstanding borrow of `bb`)
            let func_operand = func_operand_base(source_info.span);
            let tmp_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.unit, source_info.span));

            // Compute the address that the newly-created reference points to.
            // `place` is the LHS of the ref assignment (e.g., `_7` in `_7 = &_5`), so its type is `&T`.
            // We convert that pointer-like value into a stable integer address (usize) for the runtime hook.
            let addr_local = body
                .local_decls
                .push(LocalDecl::new(tcx.types.usize, source_info.span));

            let addr_stmt = Statement::new(
                source_info,
                StatementKind::Assign(Box::new((
                    Place::from(addr_local),
                    Rvalue::Cast(
                        CastKind::PointerExposeProvenance,
                        Operand::Copy(place),
                        tcx.types.usize,
                    ),
                ))),
            );

            // Prepare args: pass the computed address
            let arg_operand = Operand::Copy(Place::from(addr_local));

            let args: Box<[Spanned<Operand<'tcx>>]> = vec![Spanned {
                node: arg_operand,
                span: source_info.span,
            }]
            .into_boxed_slice();

            // Build the call terminator
            let call_term = Terminator {
                source_info,
                kind: TerminatorKind::Call {
                    func: func_operand,
                    args,
                    destination: Place::from(tmp_local),
                    target: Some(cont_block),
                    unwind: UnwindAction::Continue,
                    call_source: CallSource::Misc,
                    fn_span: source_info.span,
                },
            };

            // Split off remaining statements and set the block terminator in one borrow
            let remaining_stmts = {
                let bd: &mut BasicBlockData<'tcx> = &mut body.basic_blocks_mut()[bb];
                let rem = bd.statements.split_off(stmt_idx + 1);

                // Insert the address-computation statement right after the original ref creation.
                bd.statements.push(addr_stmt);

                // Then replace the terminator with our call.
                bd.terminator = Some(call_term);
                rem
            };

            // Extend the continuation block with the remaining statements
            body.basic_blocks_mut()[cont_block]
                .statements
                .extend(remaining_stmts);
        }

        // println!("{:#?}", body);
    }
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        let optimization_pass = MyOptimizationPass;
        optimization_pass.run_pass(tcx, &mut body);

        if let Some(path) = MIR_OUT.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let file = File::create(path).unwrap();
            let mut writer = BufWriter::new(file);
            write_mir_fn(
                tcx,
                &body,
                &mut extra,
                &mut writer,
                rustc_middle::mir::pretty::PrettyPrintMirOptions::from_cli(tcx),
            )
            .unwrap();
            writer.flush().unwrap();
        }

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

        if let Some(p) = mir_out {
            MIR_OUT.set(p).unwrap();
        }

        let runtime_path = "/Users/emanuelevannacci/github/rust-mir-instrumentor/target/release";
        args.push("-Zunstable-options".to_string());
        args.push(format!("-L{runtime_path}"));
        args.push(format!(
            "--extern=force:runtime={runtime_path}/libruntime.rlib"
        ));
        // args.push("-Zdump-mir=main".to_string());
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}
