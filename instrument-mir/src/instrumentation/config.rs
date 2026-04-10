use super::*;

impl MyOptimizationPass {
    pub(in crate::instrumentation) fn filter_stdlib_uses_enabled(&self) -> bool {
        std::env::var("RZ_FILTER_STDLIB_USES")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn warn_unknown_calls_enabled(&self) -> bool {
        std::env::var("RZ_WARN_UNKNOWN_CALLS")
            .ok()
            .map_or(true, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn heap_allocs_from_mir_enabled(&self) -> bool {
        std::env::var("RZ_HEAP_ALLOCS_FROM_MIR")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn ret_take_enabled(&self) -> bool {
        true
    }

    pub(in crate::instrumentation) fn ret_push_enabled(&self) -> bool {
        true
    }

    pub(in crate::instrumentation) fn use_storage_dead_enabled(&self) -> bool {
        std::env::var("RZ_USE_STORAGE_DEAD")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn trace_stack_allocs_enabled(&self) -> bool {
        std::env::var("RZ_TRACE_STACK_ALLOCS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_selective_enabled(&self) -> bool {
        unsafe_dataflow::unsafe_dataflow_enabled()
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_summary_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_summary_dump_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_call_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_CALL_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn unsafe_dataflow_unknown_callee_stats_enabled(&self) -> bool {
        std::env::var("RZ_UNSAFE_DATAFLOW_UNKNOWN_CALLEE_STATS")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }

    pub(in crate::instrumentation) fn analyze_unsafe_summaries_only_enabled(&self) -> bool {
        unsafe_dataflow::analyze_unsafe_summaries_enabled()
    }

    pub(in crate::instrumentation) fn trace_unsafe_dataflow_enabled(&self) -> bool {
        std::env::var("RZ_TRACE_UNSAFE_DATAFLOW")
            .ok()
            .map_or(false, |v| v != "0" && v.to_ascii_lowercase() != "false")
    }
}
