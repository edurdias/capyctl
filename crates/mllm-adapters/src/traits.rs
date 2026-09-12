use async_trait::async_trait;
use std::collections::BTreeMap;

use std::time::Duration;

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
    /// Engine build fingerprint captured at qualification/launch; adapters
    /// that cannot observe it report `None` (F1 design §3: parked-state
    /// observability contract).
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

/// The engine-side contract: inspect state, plan/park/restore, observe work.
///
/// Error semantics are explicit: `Uncertain` means the caller must reconcile;
/// success is only ever reported when the adapter knows it happened.
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError>;
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError>;
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError>;
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError>;
    async fn park(&self, member: &MemberRef, level: ParkLevel) -> Result<ParkOutcome, AdapterError>;
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

/// How a forwarded stream ended (engine-neutral; mirrors SSE semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnded {
    Completed,
    BackendClosed,
}

/// Inference forwarding: how the router reaches a deployment's engine
/// through its adapter (F1 design §5). Streaming chunk accounting lives in
/// the router; forwarders return decoded data payloads.
#[async_trait]
pub trait ChatForward: Send + Sync {
    /// Non-streaming chat completion: returns the engine's JSON response.
    async fn forward_chat(&self, body: &serde_json::Value) -> Result<serde_json::Value, AdapterError>;
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
