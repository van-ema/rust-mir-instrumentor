use super::*;

pub(in crate::instrumentation) trait FunctionDefId {
    fn func_def_id(&self) -> DefId;
}

impl FunctionDefId for DefId {
    fn func_def_id(&self) -> DefId {
        *self
    }
}

impl<'tcx> FunctionDefId for Instance<'tcx> {
    fn func_def_id(&self) -> DefId {
        self.def_id()
    }
}

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn callee_id_u64<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        def_id: DefId,
    ) -> u64 {
        tcx.def_path_hash(def_id).0.to_smaller_hash().as_u64()
    }

    pub(in crate::instrumentation) fn parse_instrumented_crates_env(
        &self,
    ) -> Option<HashSet<String>> {
        let raw = std::env::var("RZ_INSTRUMENTED_CRATES").ok()?;
        let mut set = HashSet::new();
        for part in raw.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            // Allow either '-' or '_' in names; rustc uses '_' for crate_name().
            set.insert(p.replace('-', "_"));
        }
        Some(set)
    }

    pub(in crate::instrumentation) fn is_std_like_crate_name(&self, name: &str) -> bool {
        matches!(name, "core" | "std")
    }

    pub(in crate::instrumentation) fn instrumented_crates_cached<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
    ) -> &'static HashSet<String> {
        static INSTRUMENTED: OnceLock<HashSet<String>> = OnceLock::new();

        INSTRUMENTED.get_or_init(|| {
            // Priority 1: explicit allowlist
            if let Some(env_set) = self.parse_instrumented_crates_env() {
                return env_set;
            }

            // Priority 2: instrument all non-runtime dependencies
            let instrument_all_deps = std::env::var("RZ_INSTRUMENT_ALL_DEPS")
                .ok()
                .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false");

            if !instrument_all_deps {
                return HashSet::new();
            }

            let mut set = HashSet::new();
            for &cnum in tcx.crates(()).iter() {
                let name = tcx.crate_name(cnum).as_str().to_string();
                if name == "runtime" {
                    continue;
                }

                // Even in "instrument all deps" mode, do NOT treat std/core as instrumented
                // callees. We rely on wrapper classification there.
                if self.is_std_like_crate_name(&name) {
                    continue;
                }

                set.insert(name);
            }
            set
        })
    }

    pub(in crate::instrumentation) fn maybe_print_crate_graph<'tcx>(&self, tcx: TyCtxt<'tcx>) {
        static PRINTED: OnceLock<()> = OnceLock::new();
        if PRINTED.get().is_some() {
            return;
        }

        let print = std::env::var("RZ_PRINT_CRATES")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false");

        if !print {
            return;
        }

        // Mark as printed once.
        let _ = PRINTED.set(());

        let allow = self.instrumented_crates_cached(tcx);
        eprintln!("[rusteze] crates in compilation graph:");
        for &cnum in tcx.crates(()).iter() {
            let name = tcx.crate_name(cnum).as_str().to_string();
            let flag = if allow.contains(&name) {
                "instrumented"
            } else {
                "dep"
            };
            eprintln!("  - {} ({})", name, flag);
        }
        eprintln!(
            "[rusteze] note: current crate is always treated as instrumented; set RZ_INSTRUMENTED_CRATES or RZ_INSTRUMENT_ALL_DEPS=1 to include dependencies."
        );
    }

    pub(in crate::instrumentation) fn is_instrumented_callee<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        def_id: DefId,
    ) -> bool {
        // Optionally print the crate graph once per compilation.
        self.maybe_print_crate_graph(tcx);

        // Never consider the runtime crate instrumented (avoid recursion).
        let crate_name_sym = tcx.crate_name(def_id.krate);
        let crate_name = crate_name_sym.as_str();
        if crate_name == "runtime" {
            return false;
        }
        if crate_name == "core" || crate_name == "std" {
            return false;
        }

        // Always treat the local crate as instrumented.
        if def_id.krate == LOCAL_CRATE {
            return true;
        }

        // "Instrument all deps" mode is now the default (unless explicitly disabled).
        let instrument_all_deps = std::env::var("RZ_INSTRUMENT_ALL_DEPS")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false");
        if instrument_all_deps {
            return true;
        }

        // Optional allowlist (RZ_INSTRUMENTED_CRATES) for stricter mode.
        let allow = self.instrumented_crates_cached(tcx);
        allow.contains(crate_name)
    }
}
