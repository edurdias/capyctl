use async_trait::async_trait;
use std::collections::BTreeMap;

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeAction {
    Initialize,
    Drain,
    Park,
    Restore,
    ReloadWeights,
    InvalidateCache,
    Probe,
    Stop,
    Inspect,
}

#[derive(Clone, Debug)]
pub struct RuntimeCommand {
    pub action: RuntimeAction,
    pub context: mllm_domain::completion::StepExecutionContext,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime binding is missing")]
    Missing,
    #[error("runtime binding revision is stale")]
    StaleRevision,
    #[error("runtime operation is unsupported")]
    Unsupported,
    #[error("runtime ownership is uncertain: {0}")]
    Uncertain(String),
}

/// Identifies one member of a deployment to the engine adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRef {
    pub deployment_id: String,
    pub member_id: String,
}

/// A point-in-time snapshot of an engine member's lifecycle phase and
/// memory-retention state (read by Task 9's tests: `phase`, `retained_bytes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineState {
    pub phase: Phase,
    pub retained_bytes: i64,
    /// Engine build fingerprint captured at launch; adapters that cannot
    /// observe it report `None` (F1 design §3: parked-state observability
    /// contract).
    pub build_fingerprint: Option<String>,
}

/// The shared lifecycle phase of an engine member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Ready,
    Parking,
    Parked,
    Restore,
}

/// A request to render a concrete launch command for a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInput {
    pub deployment_id: String,
    pub member_id: String,
    pub park_level: Option<ParkLevel>,
    /// Per-deployment engine credential, delivered via environment and
    /// redacted from recorded artifacts (F1 design §4).
    pub engine_api_key: Option<String>,
}

/// The concrete command a launcher can spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCommand {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Whether the engine is ready to accept work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Initializing,
    Ready,
}

/// Result of a quiescence check before parking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quiescence {
    pub quiescent: bool,
}

/// How deeply to park a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkLevel {
    /// Park level 1 (e.g. drop KV cache only).
    One,
    /// Park level 2 (e.g. drop KV cache and weights).
    Two,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkOutcome {
    Parked { retained_bytes: i64 },
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    Reloaded,
    Failed,
}

/// Observation of in-flight work on a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkObservation {
    Idle,
    Streaming { request_ref: String },
    Unknown,
}

/// Identifies a request whose work may need cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRef {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationOutcome {
    Acknowledged,
    Uncertain,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    /// The operation may or may not have taken effect; the caller must reconcile.
    Uncertain(String),
    /// The operation is denied by policy, deterministically.
    PolicyDenied,
    /// The engine does not implement this capability at all.
    UnsupportedCapability,
    /// The engine crashed while in the given phase.
    Crash(Phase),
    /// The combination of arguments/states is not supported.
    UnsupportedCombination,
}

/// Spec §3: an adapter error is quoted into failure reasons that reach a journal,
/// and the caller adds its own prefix when it does. Debug would nest the variant
/// name and its quoting inside that prefix, so `Uncertain` renders as its reason
/// alone and the other variants as the plain thing they mean.
impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::Uncertain(reason) => f.write_str(reason),
            AdapterError::PolicyDenied => f.write_str("denied by policy"),
            AdapterError::UnsupportedCapability => {
                f.write_str("the engine does not implement this capability")
            }
            AdapterError::Crash(phase) => write!(f, "the engine crashed during {phase:?}"),
            AdapterError::UnsupportedCombination => {
                f.write_str("unsupported combination of arguments or states")
            }
        }
    }
}

/// The engine-side contract: inspect state, plan/park/restore, observe work.
///
/// Error semantics are explicit: `Uncertain` means the caller must reconcile;
/// success is only ever reported when the adapter knows it happened.
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    /// One persisted child effect only. Implementations must not hide legacy
    /// compound Restore/reload/probe behavior behind this entry point.
    async fn execute_persisted(
        &self,
        _command: &RuntimeCommand,
    ) -> Result<mllm_domain::completion::EffectObservation, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError>;
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError>;
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError>;
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError>;
    async fn park(&self, member: &MemberRef, level: ParkLevel)
    -> Result<ParkOutcome, AdapterError>;
    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError>;
    async fn reload_weights(&self, member: &MemberRef) -> Result<ReloadOutcome, AdapterError>;
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError>;
    async fn cancel_work(
        &self,
        member: &MemberRef,
        req: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError>;
}

/// A boot-unique handle to a spawned engine process.
///
/// `start_identity` makes PID reuse detectable: it must be drawn from a value
/// that changes across process starts (e.g. procfs starttime + a boot id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnedHandle {
    pub pid: u32,
    pub start_identity: u128,
}

/// How a terminated process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub killed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleStatus {
    /// The handle still refers to the process we spawned.
    Valid,
    /// The PID exists but its start identity differs — PID reuse detected.
    StaleReused,
    /// No such process.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherError {
    SpawnFailed(String),
    TerminateFailed(String),
}

/// The process-side contract: spawn, terminate with grace, verify ownership.
pub trait Launcher: Send + Sync {
    fn spawn(&self, cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError>;
    fn terminate(&self, h: &OwnedHandle, grace: Duration) -> Result<ExitReport, LauncherError>;
    /// Detects PID reuse by comparing the process's current start identity
    /// with the one recorded in the handle.
    fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus;
}

/// Process tools a builder uses on Initialize and Cleanup. The director supplies
/// them; the builder never learns where identities are recorded (spec §3).
///
/// Synchronous on purpose: `mllm-launchers` has no async runtime. Builders call the
/// blocking methods through `tokio::task::spawn_blocking`.
pub trait OwnedProcessLaunch: Send + Sync {
    /// Spawn gated: the child runs only after its identity is durable. The engine's
    /// stdout and stderr go to the file named by `cmd.env["MLLM_ENGINE_LOG"]`.
    /// A child that is never released is disposed of before this returns an error.
    fn spawn_durable(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<mllm_domain::completion::ProcessIdentity, RuntimeError>;
    /// Live now, with the same start identity: boot id and start ticks, not pid alone.
    fn present(&self, identity: &mllm_domain::completion::ProcessIdentity)
        -> mllm_domain::completion::Presence;
    /// Every live member of the process group the recorded API process led: the API
    /// process first when it is still live, workers named `worker-0`, `worker-1`, ...
    /// in start order, and an empty list when no member is live. Empty is an answer,
    /// not an error; cleanup depends on it.
    fn observe_group(
        &self,
        api: &mllm_domain::completion::ProcessIdentity,
    ) -> Result<Vec<mllm_domain::completion::ProcessIdentity>, RuntimeError>;
    /// SIGTERM the owned group, wait `grace`, SIGKILL, then prove every identity gone
    /// and the group itself empty. Refuses to signal a pid whose start identity
    /// differs from the recorded one (SPEC §13.2: never kill a process you cannot
    /// prove you own).
    fn terminate_owned(
        &self,
        identities: &[mllm_domain::completion::ProcessIdentity],
        grace: std::time::Duration,
    ) -> Result<(), RuntimeError>;
}

/// How a forwarded stream ended (engine-neutral; mirrors SSE semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnded {
    Completed,
    BackendClosed,
}

/// Downstream delivery failed; this says nothing about backend completion.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryFailed;

/// A bounded, backpressure-aware destination for one decoded payload.
#[async_trait]
pub trait ChatSink: Send {
    async fn send(&mut self, chunk: String) -> Result<(), DeliveryFailed>;
}

/// Inference forwarding: how the router reaches a deployment's engine
/// through its adapter (F1 design §5). Streaming chunk accounting lives in
/// the router; forwarders return decoded data payloads.
#[async_trait]
pub trait ChatForward: Send + Sync {
    /// Await delivery in order. On sink failure or timeout, stop delivery and
    /// drain the backend within its existing limits. Completed describes only
    /// the backend protocol terminator, never successful downstream delivery.
    /// No synchronous fallback: implementations must explicitly support this.
    async fn forward_chat_stream_async(
        &self,
        _body: &serde_json::Value,
        _sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    /// Non-streaming chat completion: returns the engine's JSON response.
    async fn forward_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError>;
    /// Streaming chat completion: yields data payloads in order; the final
    /// `[DONE]` marker is consumed by the forwarder.
    async fn forward_chat_stream(
        &self,
        body: &serde_json::Value,
        _on_chunk: &mut (dyn FnMut(String) + Send),
    ) -> Result<StreamEnded, AdapterError> {
        let _ = body;
        Err(AdapterError::UnsupportedCapability)
    }
}
