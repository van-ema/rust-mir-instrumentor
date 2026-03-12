use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::OnceLock;

use rustc_hir::def_id::LocalDefId;
use rustc_interface::Config;
use rustc_middle::mir::pretty::write_mir_fn;
use rustc_middle::mir::Body;
use rustc_middle::ty::TyCtxt;

use crate::instrumentation::MyOptimizationPass;

static MIR_OUT_BEFORE: OnceLock<String> = OnceLock::new();
static MIR_OUT_AFTER: OnceLock<String> = OnceLock::new();

pub(crate) fn set_mir_output_paths(before: String, after: String) {
    MIR_OUT_BEFORE.set(before).unwrap();
    MIR_OUT_AFTER.set(after).unwrap();
}

const CUSTOM_OPT_MIR: for<'tcx> fn(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Body<'tcx> =
    |tcx, def| {
        let mut body = (rustc_interface::DEFAULT_QUERY_PROVIDERS.optimized_mir)(tcx, def).clone();

        // Write MIR before running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_BEFORE.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let def_path = tcx.def_path_str(body.source.def_id());

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let mut writer = BufWriter::new(file);

            writeln!(&mut writer, "\n\n// ===== MIR BEFORE: {} =====", def_path).unwrap();

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

        let optimization_pass = MyOptimizationPass;
        optimization_pass.run_pass(tcx, &mut body);

        // Write MIR after running our optimization/instrumentation.
        if let Some(path) = MIR_OUT_AFTER.get() {
            let mut extra = |_, _: &mut dyn std::io::Write| Ok(());
            let def_path = tcx.def_path_str(body.source.def_id());

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            let mut writer = BufWriter::new(file);

            writeln!(&mut writer, "\n\n// ===== MIR AFTER: {} =====", def_path).unwrap();

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

pub(crate) struct CompilerCallbacks;

impl rustc_driver::Callbacks for CompilerCallbacks {
    fn config(&mut self, _config: &mut Config) {
        _config.override_queries = Some(|_session, queries| {
            queries.optimized_mir = CUSTOM_OPT_MIR;
        });
    }
}

pub(crate) struct NoopCallbacks;

impl rustc_driver::Callbacks for NoopCallbacks {}
