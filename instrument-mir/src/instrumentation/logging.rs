use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn log_level(&self) -> PassLogLevel {
        match std::env::var("RZ_LOG")
            .unwrap_or_else(|_| "warn".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "trace" => PassLogLevel::Trace,
            "info" => PassLogLevel::Info,
            _ => PassLogLevel::Warn,
        }
    }

    pub(in crate::instrumentation) fn log_enabled(&self, level: PassLogLevel) -> bool {
        self.log_level() >= level
    }

    pub(in crate::instrumentation) fn trace_stack_alloc_emit<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        body: &Body<'tcx>,
        local: Local,
        live: bool,
        size_op: &SizeOperand<'tcx>,
        source: &str,
    ) {
        if !self.trace_stack_allocs_enabled() {
            return;
        }
        let def_path = tcx.def_path_str(body.source.def_id());
        rz_pass_warn!(
            self,
            "[rusteze][trace-stack] fn={} local=_{} live={} size_op={:?} source={}",
            def_path,
            local.index(),
            live,
            size_op,
            source
        );
    }

    pub(in crate::instrumentation) fn warn_unknown_call_once(&self, def_path: &str) {
        static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut guard = set.lock().unwrap();
        if guard.insert(def_path.to_string()) {
            if self.log_enabled(PassLogLevel::Trace) {
                static TRACE_UNKNOWN: OnceLock<Mutex<usize>> = OnceLock::new();
                let mut count = TRACE_UNKNOWN.get_or_init(|| Mutex::new(0)).lock().unwrap();
                if *count < 10 {
                    *count += 1;
                    rz_pass_warn!(
                        self,
                        "[rusteze][trace] unknown_call def_path={:?} contains_slice_impl={} ends_get={} ends_get_mut={} ends_is_empty={}",
                        def_path,
                        def_path.contains("::slice::<impl ["),
                        def_path.ends_with("::get"),
                        def_path.ends_with("::get_mut"),
                        def_path.ends_with("::is_empty")
                    );
                }
            }
            if std::env::var("RZ_TRACE_UNKNOWN_CALLS")
                .ok()
                .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
            {
                static TRACE_UNKNOWN_DETAILS: OnceLock<Mutex<usize>> = OnceLock::new();
                let mut count = TRACE_UNKNOWN_DETAILS
                    .get_or_init(|| Mutex::new(0))
                    .lock()
                    .unwrap();
                if *count < 20 {
                    *count += 1;
                    rz_pass_warn!(
                        self,
                        "[rusteze][trace] unknown_call_details def_path={:?} bytes={:?} contains_fmt={} contains_slice_impl={} contains_vec={} contains_index={} ends_index={} ends_index_mut={}",
                        def_path,
                        def_path.as_bytes(),
                        def_path.contains("::fmt::"),
                        def_path.contains("::slice::<impl ["),
                        def_path.contains("::vec::Vec"),
                        def_path.contains("::ops::Index"),
                        def_path.ends_with("::index"),
                        def_path.ends_with("::index_mut")
                    );
                }
            }
            rz_pass_warn!(
                self,
                "[rusteze][warn] unclassified direct call with pointer effects: {} (consider adding a wrapper/intrinsic classifier or instrumenting that crate)",
                def_path
            );
        }
    }
}
