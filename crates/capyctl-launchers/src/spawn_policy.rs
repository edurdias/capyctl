//! ADR 0033 §10: what a launch's processes start with besides their rendered
//! command, decided per launch by its builder (both from
//! `LmcachePlan::retention_zero`), never by the host's process-wide settings
//! alone.

/// How every process of one launch is started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpawnPolicy {
    /// ADR 0033 §10: start with `RLIMIT_CORE` 0 (soft and hard), so a crash
    /// writes no core dump of request data.
    pub no_core_dumps: bool,
    /// ADR 0033 §10: keep the launch's output in its log writer's memory
    /// (`engine_logs: memory`) whatever the host's `engine_logs` says.
    pub memory_logs: bool,
}
