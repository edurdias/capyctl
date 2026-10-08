//! ADR 0014 §7 (WE3): measuring and recording checkpoint digests.
//!
//! A deployment is accepted with its digest pending. This supervisor asks the
//! host that holds the checkpoint to measure it (remote: the `DigestCheckpoint`
//! member action; embedded: the same measurement in-process) and records the
//! result, so the digest is normally recorded shortly after deploy and the
//! host's stat cache is warm before the first launch. A launch whose digest is
//! still pending measures it first, before anything is sent (first placement).
//! Every launch and wake then carries the recorded digest and the host refuses
//! a checkpoint that does not measure to it.
//!
//! Nothing here launches, releases or reserves anything. A host that cannot be
//! reached leaves the digest pending; a refusal is kept as a closed category.
use crate::{agent_sessions::AgentSessions, ownership::SharedCoordinatorState};
use capyctl_adapters::traits::*;
use capyctl_agent::checkpoint::CheckpointVerifier;
use capyctl_domain::{
    completion::EffectObservation,
    group::{CommandIdentity, MemberKey},
};
use capyctl_protocol::{
    execution::{DigestCheckpointPlan, MemberAction, MemberCommand},
    pb,
};
use capyctl_store::checkpoint_digests::{PendingDigest, RecordOutcome};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// How often the supervisor looks for pending digests.
const PASS_INTERVAL: Duration = Duration::from_secs(1);
/// The bound on one remote measurement: a first placement hashes the whole
/// checkpoint (about 30 s for 60 GB at 2 GB/s; slower disks take longer).
pub const DIGEST_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// The bound on one remote sizing: a stat walk, no hashing.
const SIZE_DEADLINE: Duration = Duration::from_secs(60);
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(300);

/// A measured checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    pub digest: String,
    pub weights_bytes: i64,
    /// ADR 0014 amendment A16: the hybrid state slot read beside the weights.
    pub state_slot_bytes: Option<i64>,
    /// ADR 0028 §5 (amendment of 2026-10-07): the checkpoint's layout, which a
    /// group member's share of the weights is taken with.
    pub layout: Option<capyctl_domain::member_weights::CheckpointLayout>,
}

/// Why a measurement produced no digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeasureError {
    /// The host refused, with a closed category.
    Refused(String),
    /// No answer: the host is offline or the command did not complete.
    Unavailable,
}

pub type MeasureFuture = Pin<Box<dyn Future<Output = Result<Measured, MeasureError>> + Send>>;

/// The weights a sizing found (owner decision 2026-09-23, solo first start).
pub type SizeFuture = Pin<Box<dyn Future<Output = Result<i64, MeasureError>> + Send>>;

/// Where a pending digest is measured.
pub trait DigestSource: Send + Sync + 'static {
    /// Whether the host can be asked now (remote: a reconciled session).
    fn reachable(&self, host: &str) -> bool;
    fn measure(&self, pending: PendingDigest) -> MeasureFuture;
    /// Owner decision 2026-09-23 (solo first start): size the checkpoint's
    /// weight files without hashing, so a first start's startup estimate is
    /// known before the digest. A source that cannot size leaves it unknown.
    fn size(&self, _pending: PendingDigest) -> SizeFuture {
        Box::pin(async { Err(MeasureError::Unavailable) })
    }
}

/// What a DigestCheckpoint result proves: a `computed` digest with its
/// weights, or a closed refusal. `validate_result` already bound its shape.
pub fn measured_from(result: &pb::MemberExecutionResult) -> Result<Measured, MeasureError> {
    let evidence = result
        .checkpoint
        .as_ref()
        .ok_or(MeasureError::Unavailable)?;
    match evidence.state.as_str() {
        // A mismatch against an expectation is still a measurement; the store
        // compares it with what it expects and records the mismatch.
        "computed" | "mismatch" => Ok(Measured {
            digest: evidence.digest.clone(),
            weights_bytes: evidence.weights_bytes,
            state_slot_bytes: evidence.state_slot_bytes,
            layout: evidence
                .layout
                .as_ref()
                .and_then(capyctl_protocol::execution::layout_from_wire),
        }),
        "refused" => Err(MeasureError::Refused(evidence.reason.clone())),
        _ => Err(MeasureError::Unavailable),
    }
}

/// What a size-only DigestCheckpoint result proves: the weights, or a closed
/// refusal.
pub fn sized_from(result: &pb::MemberExecutionResult) -> Result<i64, MeasureError> {
    let evidence = result
        .checkpoint
        .as_ref()
        .ok_or(MeasureError::Unavailable)?;
    match evidence.state.as_str() {
        "sized" => Ok(evidence.weights_bytes),
        "refused" => Err(MeasureError::Refused(evidence.reason.clone())),
        _ => Err(MeasureError::Unavailable),
    }
}

/// Record a measurement under the owner. The recorded digest is returned only
/// when this measurement is (now) the revision's recorded digest.
pub fn record(
    owner: &SharedCoordinatorState,
    deployment: &str,
    revision: i64,
    host: &str,
    measured: &Measured,
) -> Result<RecordOutcome, MeasureError> {
    let owner = owner.lock().map_err(|_| MeasureError::Unavailable)?;
    owner
        .store()
        .record_checkpoint_measurement(
            owner.session(),
            deployment,
            revision,
            host,
            &measured.digest,
            measured.weights_bytes,
            measured.state_slot_bytes,
            measured.layout,
            capyctl_protocol::now_unix_ms(),
        )
        .map_err(|_| MeasureError::Unavailable)
}

/// ADR 0014 §7 (WE3): the digest a first placement launches with.
///
/// The host that holds the checkpoint measures it (`measure`) and the server
/// records the measurement under the revision's declared expectation before
/// anything is sent. A measurement that is not the declared canonical
/// `content_fingerprint` (or the digest already recorded) is a mismatch: the
/// launch is refused with `checkpoint_mismatch` before any effect, as a wake
/// is (SPEC §13; found live by the M48 soak on 2026-09-24, where a declared
/// fingerprint the checkpoint did not measure to was reported as uncertain
/// runtime ownership). A measurement that cannot be made or recorded stays
/// uncertain: the launch is not sent and nothing is released on it here.
pub async fn first_placement_digest<F, Fut>(
    owner: &SharedCoordinatorState,
    deployment: &str,
    revision: i64,
    host: &str,
    measure: F,
) -> Result<String, RuntimeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Measured, MeasureError>>,
{
    let unrecorded =
        || RuntimeError::Uncertain("the checkpoint digest was not recorded before launch".into());
    let measured = measure().await.map_err(|error| match error {
        MeasureError::Refused(code) => {
            RuntimeError::Uncertain(format!("the checkpoint could not be measured ({code})"))
        }
        MeasureError::Unavailable => unrecorded(),
    })?;
    match record(owner, deployment, revision, host, &measured).map_err(|_| unrecorded())? {
        RecordOutcome::Recorded { digest, .. } if digest == measured.digest => Ok(digest),
        RecordOutcome::Mismatch => Err(RuntimeError::Refused("checkpoint_mismatch".into())),
        _ => Err(unrecorded()),
    }
}

/// ADR 0014 §7, owner decision 5 (2026-09-22): the digest a wake must carry.
///
/// A revision whose digest is recorded carries it. One with none recorded (a
/// launch parked before checkpoint digests existed, which WE3 left unwakeable,
/// or a revision whose digest is still pending) is first measured on the host
/// that holds the checkpoint (`measure`, the WE3 digest path, a full hash) and
/// recorded under the same validation as a first placement: a declared
/// canonical `content_fingerprint` must equal it. A mismatch refuses the wake
/// with `checkpoint_mismatch`; a measurement that cannot be made or recorded
/// refuses it without effect (`Unsupported`). Either way nothing was sent to
/// the engine and the launch stays parked.
pub async fn wake_digest<F, Fut>(
    owner: &SharedCoordinatorState,
    deployment: &str,
    revision: i64,
    host: &str,
    measure: F,
) -> Result<String, RuntimeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Measured, MeasureError>>,
{
    let recorded = {
        let owner = owner.lock().map_err(|_| RuntimeError::Unsupported)?;
        owner
            .store()
            .recorded_checkpoint(deployment, revision)
            .map_err(|_| RuntimeError::Unsupported)?
    };
    if let Some(digest) = recorded {
        return Ok(digest);
    }
    let measured = measure().await.map_err(|_| RuntimeError::Unsupported)?;
    match record(owner, deployment, revision, host, &measured)
        .map_err(|_| RuntimeError::Unsupported)?
    {
        RecordOutcome::Recorded { digest, .. } if digest == measured.digest => Ok(digest),
        _ => Err(RuntimeError::Refused("checkpoint_mismatch".into())),
    }
}

/// The `DigestCheckpoint` command for one deployment revision on one host,
/// addressed to `member_id` there (ADR 0028 §6: `head` for a single-host
/// deployment, the host's own member for a group).
#[allow(clippy::too_many_arguments)]
pub fn digest_command(
    controller_id: &str,
    host_id: &str,
    member_id: &str,
    deployment_id: &str,
    revision: i64,
    generation: i64,
    profile_fingerprint: &str,
    deployment_config: String,
    host_policy_fingerprint: String,
    deadline_ms: i64,
) -> MemberCommand {
    checkpoint_command(
        controller_id,
        host_id,
        member_id,
        deployment_id,
        revision,
        generation,
        profile_fingerprint,
        deployment_config,
        host_policy_fingerprint,
        deadline_ms,
        false,
    )
}

/// As [`digest_command`], or a size-only request (`size_only`).
#[allow(clippy::too_many_arguments)]
fn checkpoint_command(
    controller_id: &str,
    host_id: &str,
    member_id: &str,
    deployment_id: &str,
    revision: i64,
    generation: i64,
    profile_fingerprint: &str,
    deployment_config: String,
    host_policy_fingerprint: String,
    deadline_ms: i64,
    size_only: bool,
) -> MemberCommand {
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: controller_id.into(),
            member: MemberKey {
                host_id: host_id.into(),
                member_id: member_id.into(),
            },
            deployment_id: deployment_id.into(),
            operation_id: id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation,
            revision,
            deadline_ms,
            payload_digest: [0; 32],
            expected_state: "checkpoint".into(),
            profile_fingerprint: profile_fingerprint.into(),
            instance_index: 0,
        },
        action: MemberAction::DigestCheckpoint(DigestCheckpointPlan {
            deployment_config,
            host_policy_fingerprint,
            expected_digest: None,
            size_only,
        }),
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// Remote hosts, through their authenticated sessions.
pub struct RemoteDigests {
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    controller_id: String,
}

impl RemoteDigests {
    pub fn new(
        owner: SharedCoordinatorState,
        sessions: Arc<AgentSessions>,
        controller_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            sessions,
            controller_id,
        })
    }

    fn command(&self, pending: &PendingDigest) -> Result<MemberCommand, MeasureError> {
        self.command_for(pending, false)
    }

    fn command_for(
        &self,
        pending: &PendingDigest,
        size_only: bool,
    ) -> Result<MemberCommand, MeasureError> {
        let owner = self.owner.lock().map_err(|_| MeasureError::Unavailable)?;
        let publication = owner
            .store()
            .host_publication(&pending.host_id)
            .ok()
            .flatten()
            .filter(|publication| publication.host_id == pending.host_id)
            .ok_or(MeasureError::Unavailable)?;
        let source = owner
            .store()
            .host_configuration_source(&pending.deployment_id, pending.revision, &pending.host_id)
            .ok()
            .flatten()
            .ok_or(MeasureError::Unavailable)?;
        let local =
            capyctl_config::remote_resources::local_deployment_document(&pending.host_id, &source)
                .map_err(|_| MeasureError::Refused("unauthorized".into()))?;
        Ok(checkpoint_command(
            &self.controller_id,
            &pending.host_id,
            "head",
            &pending.deployment_id,
            pending.revision,
            pending.generation,
            &pending.effective.profile.build_fingerprint,
            local.to_string(),
            publication.fingerprint,
            capyctl_protocol::now_unix_ms()
                + if size_only {
                    SIZE_DEADLINE
                } else {
                    DIGEST_DEADLINE
                }
                .as_millis() as i64,
            size_only,
        ))
    }
}

impl DigestSource for RemoteDigests {
    fn reachable(&self, host: &str) -> bool {
        // ADR 0017: a drain-only host, or one without the digest action, is
        // never asked; its digest stays pending until it is upgraded.
        self.sessions.current_session(host).is_some()
            && self
                .sessions
                .preflight(
                    host,
                    &[capyctl_protocol::capabilities::CHECKPOINT_DIGEST],
                    true,
                )
                .is_ok()
    }
    fn measure(&self, pending: PendingDigest) -> MeasureFuture {
        let command = self.command(&pending);
        let sessions = self.sessions.clone();
        Box::pin(async move {
            let result = sessions
                .execute(command?)
                .await
                .map_err(|_| MeasureError::Unavailable)?;
            measured_from(&result)
        })
    }
    fn size(&self, pending: PendingDigest) -> SizeFuture {
        let command = self.command_for(&pending, true);
        let sessions = self.sessions.clone();
        Box::pin(async move {
            let result = sessions
                .execute(command?)
                .await
                .map_err(|_| MeasureError::Unavailable)?;
            sized_from(&result)
        })
    }
}

/// The embedded host: the same measurement, in this process.
pub struct LocalDigests {
    checkpoints: Arc<CheckpointVerifier>,
}

impl LocalDigests {
    pub fn new(checkpoints: Arc<CheckpointVerifier>) -> Arc<Self> {
        Arc::new(Self { checkpoints })
    }
}

/// The checkpoint's verification, its weights with the draft model's, its
/// hybrid state slot and its layout.
type LocalMeasurement = (
    capyctl_agent::checkpoint::Verification,
    i64,
    Option<i64>,
    Option<capyctl_domain::member_weights::CheckpointLayout>,
);

/// Measure an effective revision's checkpoint on this machine, with the
/// weight bytes of the draft model it loads beside it (ADR 0014 §5 amendment
/// A4) and the hybrid state slot (amendment A16). The digest is the
/// checkpoint's own.
async fn measure_locally(
    checkpoints: Arc<CheckpointVerifier>,
    effective: &capyctl_config::effective::EffectiveDeployment,
) -> Result<LocalMeasurement, MeasureError> {
    let store = effective.checkpoint_store().to_path_buf();
    let checkpoint = effective
        .model
        .require_resolved_path()
        .map_err(|_| MeasureError::Refused("not_materializable".into()))?
        .to_owned();
    let drafter = effective.drafter_location();
    let effective = effective.clone();
    tokio::task::spawn_blocking(move || {
        let verified = checkpoints.measure(&store, Path::new(&checkpoint))?;
        let weights = checkpoints
            .drafter_weights(drafter.as_ref())?
            .checked_add(verified.manifest.weights_bytes)
            .ok_or(capyctl_agent::checkpoint::CheckpointError::TooLarge)?;
        Ok((
            verified,
            weights,
            effective.state_slot_bytes(),
            effective.checkpoint_layout(),
        ))
    })
    .await
    .map_err(|_| MeasureError::Unavailable)?
    .map_err(|error: capyctl_agent::checkpoint::CheckpointError| {
        MeasureError::Refused(error.code().into())
    })
}

impl DigestSource for LocalDigests {
    fn reachable(&self, _: &str) -> bool {
        true
    }
    fn measure(&self, pending: PendingDigest) -> MeasureFuture {
        let checkpoints = self.checkpoints.clone();
        Box::pin(async move {
            let (verified, weights_bytes, state_slot_bytes, layout) =
                measure_locally(checkpoints, &pending.effective).await?;
            Ok(Measured {
                digest: verified.manifest.digest,
                weights_bytes,
                state_slot_bytes,
                layout,
            })
        })
    }
    fn size(&self, pending: PendingDigest) -> SizeFuture {
        let checkpoints = self.checkpoints.clone();
        Box::pin(async move {
            let store = pending.effective.checkpoint_store().to_path_buf();
            let checkpoint = pending
                .effective
                .model
                .require_resolved_path()
                .map_err(|_| MeasureError::Refused("not_materializable".into()))?
                .to_owned();
            let drafter = pending.effective.drafter_location();
            tokio::task::spawn_blocking(move || {
                let size = checkpoints.size(&store, Path::new(&checkpoint))?;
                checkpoints
                    .drafter_weights(drafter.as_ref())?
                    .checked_add(size.weights_bytes)
                    .ok_or(capyctl_agent::checkpoint::CheckpointError::TooLarge)
            })
            .await
            .map_err(|_| MeasureError::Unavailable)?
            .map_err(|error| MeasureError::Refused(error.code().into()))
        })
    }
}

struct Attempt {
    next_at: Instant,
    backoff: Duration,
}

/// The background supervisor: at most one measurement per host at a time
/// (each is one host effect), with backoff after a failure.
pub struct CheckpointDigests {
    owner: SharedCoordinatorState,
    source: Arc<dyn DigestSource>,
    running: Mutex<BTreeSet<String>>,
    attempts: Mutex<BTreeMap<(String, i64), Attempt>>,
}

impl CheckpointDigests {
    pub fn new(owner: SharedCoordinatorState, source: Arc<dyn DigestSource>) -> Arc<Self> {
        Arc::new(Self {
            owner,
            source,
            running: Mutex::default(),
            attempts: Mutex::default(),
        })
    }

    /// Run supervision until the task is aborted. Aborting it aborts every
    /// measurement it started (review finding 15: they held the owned state).
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.spawn_until(crate::supervised::never())
    }

    /// Run supervision until `cancel` reads true (or the task is aborted).
    /// On cancel, every measurement in flight is aborted and joined before
    /// the task returns. An aborted measurement records nothing; the digest
    /// stays pending for the next run.
    pub fn spawn_until(
        self: Arc<Self>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let children = Arc::new(crate::supervised::Children::default());
            let aborting = children.clone();
            let _abort = crate::supervised::OnDrop(move || aborting.abort_all());
            let mut tick = tokio::time::interval(PASS_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => break,
                    _ = tick.tick() => {}
                }
                for measurement in self.clone().pass() {
                    children.track(measurement);
                }
            }
            children.join_all().await;
        })
    }

    /// One pass: start a measurement for every due pending digest whose host
    /// is reachable and has none running.
    pub fn pass(self: Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let pending = match self.owner.lock() {
            Ok(owner) => owner
                .store()
                .pending_checkpoint_digests()
                .unwrap_or_default(),
            Err(_) => return Vec::new(),
        };
        let now = Instant::now();
        let mut started = Vec::new();
        for digest in pending {
            let key = (digest.deployment_id.clone(), digest.revision);
            let due = self
                .attempts
                .lock()
                .map(|attempts| attempts.get(&key).is_none_or(|a| a.next_at <= now))
                .unwrap_or(false);
            if !due || !self.source.reachable(&digest.host_id) {
                continue;
            }
            let Ok(mut running) = self.running.lock() else {
                continue;
            };
            if !running.insert(digest.host_id.clone()) {
                continue;
            }
            drop(running);
            let this = self.clone();
            started.push(tokio::spawn(async move {
                let host = digest.host_id.clone();
                let key = (digest.deployment_id.clone(), digest.revision);
                // Owner decision 2026-09-23 (solo first start): size the
                // weights first (a stat walk), so a first start's startup
                // estimate is known within seconds rather than after the full
                // hash. A sizing that fails changes nothing; the digest follows.
                if digest.weights_bytes.is_none() {
                    if let Ok(weights) = this.source.size(digest.clone()).await {
                        if let Ok(owner) = this.owner.lock() {
                            let _ = owner.store().record_checkpoint_weights(
                                owner.session(),
                                &digest.deployment_id,
                                digest.revision,
                                &host,
                                weights,
                                capyctl_protocol::now_unix_ms(),
                            );
                        }
                    }
                }
                let outcome = this.source.measure(digest.clone()).await;
                let failed = match &outcome {
                    Ok(measured) => record(
                        &this.owner,
                        &digest.deployment_id,
                        digest.revision,
                        &host,
                        measured,
                    )
                    .is_err(),
                    Err(MeasureError::Refused(reason)) => {
                        if let Ok(owner) = this.owner.lock() {
                            let _ = owner.store().note_checkpoint_digest_refusal(
                                owner.session(),
                                &digest.deployment_id,
                                digest.revision,
                                reason,
                                capyctl_protocol::now_unix_ms(),
                            );
                        }
                        true
                    }
                    Err(MeasureError::Unavailable) => true,
                };
                if let Ok(mut attempts) = this.attempts.lock() {
                    if failed {
                        let backoff = attempts
                            .get(&key)
                            .map_or(FIRST_RETRY, |a| (a.backoff * 2).min(MAX_RETRY));
                        attempts.insert(
                            key,
                            Attempt {
                                next_at: Instant::now() + backoff,
                                backoff,
                            },
                        );
                    } else {
                        attempts.remove(&key);
                    }
                }
                if let Ok(mut running) = this.running.lock() {
                    running.remove(&host);
                }
            }));
        }
        started
    }
}

/// ADR 0014 §7 (WE3), embedded path: the same verification a remote host runs,
/// around the embedded engine adapter. Before Initialize and before Restore the
/// binding's checkpoint must measure to the recorded digest; a digest not yet
/// recorded is measured and recorded first (first placement). A refusal
/// happens before the engine is asked to do anything.
pub struct CheckpointGate {
    inner: Arc<dyn EngineAdapter>,
    owner: SharedCoordinatorState,
    checkpoints: Arc<CheckpointVerifier>,
    effective: capyctl_config::effective::EffectiveDeployment,
    deployment_id: String,
    revision: i64,
}

impl CheckpointGate {
    pub fn new(
        inner: Arc<dyn EngineAdapter>,
        owner: SharedCoordinatorState,
        checkpoints: Arc<CheckpointVerifier>,
        work: &capyctl_store::ordinary_lifecycle::worker::InitializeWork,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            owner,
            checkpoints,
            effective: work.effective().clone(),
            deployment_id: work.fence().deployment_id.clone(),
            revision: work.fence().revision,
        })
    }

    /// ADR 0014 §7: before Initialize a failure to verify is uncertain; the
    /// message keeps the walker's reason.
    async fn verified(&self) -> Result<(), RuntimeError> {
        self.check().await.map_err(|refusal| match refusal {
            // Found live 2026-10-04: the closed code leads, so status and
            // `--wait` attach the hint about model.content_fingerprint.
            GateRefusal::Mismatch => RuntimeError::Uncertain(
                "checkpoint_mismatch: the checkpoint does not match its recorded digest".into(),
            ),
            GateRefusal::Unavailable(reason) => {
                RuntimeError::Uncertain(format!("the checkpoint could not be measured ({reason})"))
            }
        })
    }

    /// Owner decision 5 (2026-09-22): before a wake, a checkpoint known not to
    /// be the recorded (or declared) one refuses it with `checkpoint_mismatch`;
    /// one that cannot be measured or recorded refuses it without effect.
    /// Nothing reached the engine either way; the launch stays parked.
    async fn verified_wake(&self) -> Result<(), RuntimeError> {
        self.check().await.map_err(|refusal| match refusal {
            GateRefusal::Mismatch => RuntimeError::Refused("checkpoint_mismatch".into()),
            GateRefusal::Unavailable(_) => RuntimeError::Unsupported,
        })
    }

    /// Measure the checkpoint and compare it with the recorded digest; a
    /// revision with none recorded (first placement, or a launch parked before
    /// digests existed) records this measurement first, validated as usual.
    async fn check(&self) -> Result<(), GateRefusal> {
        let unrecorded = || GateRefusal::Unavailable("not_recorded".into());
        let recorded = {
            let owner = self.owner.lock().map_err(|_| unrecorded())?;
            owner
                .store()
                .recorded_checkpoint(&self.deployment_id, self.revision)
                .map_err(|_| unrecorded())?
        };
        let (measured, weights_bytes, state_slot_bytes, layout) =
            measure_locally(self.checkpoints.clone(), &self.effective)
                .await
                .map_err(|error| match error {
                    MeasureError::Refused(code) => GateRefusal::Unavailable(code),
                    MeasureError::Unavailable => GateRefusal::Unavailable("unavailable".into()),
                })?;
        match recorded {
            Some(digest) if digest == measured.manifest.digest => Ok(()),
            Some(_) => Err(GateRefusal::Mismatch),
            None => {
                let outcome = record(
                    &self.owner,
                    &self.deployment_id,
                    self.revision,
                    &self.effective.host.name,
                    &Measured {
                        digest: measured.manifest.digest.clone(),
                        weights_bytes,
                        state_slot_bytes,
                        layout,
                    },
                )
                .map_err(|_| unrecorded())?;
                match outcome {
                    RecordOutcome::Recorded { digest, .. }
                        if digest == measured.manifest.digest =>
                    {
                        Ok(())
                    }
                    _ => Err(GateRefusal::Mismatch),
                }
            }
        }
    }
}

/// Why the embedded gate refused a checkpoint.
enum GateRefusal {
    /// The checkpoint is known not to be the recorded or declared one.
    Mismatch,
    /// It could not be measured or recorded now, with the reason.
    Unavailable(String),
}

#[async_trait::async_trait]
impl EngineAdapter for CheckpointGate {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => self.verified().await?,
            // Owner decision 5: a launch parked before digests existed is
            // measured and recorded first, then woken.
            RuntimeAction::Restore => self.verified_wake().await?,
            _ => {}
        }
        self.inner.execute_persisted(command).await
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        self.inner.inspect(member).await
    }
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        self.inner.render_plan(plan).await
    }
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError> {
        self.inner.check_readiness(member).await
    }
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        self.inner.prepare_park(member).await
    }
    async fn park(
        &self,
        member: &MemberRef,
        level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        self.inner.park(member, level).await
    }
    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        // Wake (SPEC §9.1) reloads weights from disk: verify first.
        self.verified()
            .await
            .map_err(|_| AdapterError::PolicyDenied)?;
        self.inner.restore(member).await
    }
    async fn reload_weights(&self, member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        self.inner.reload_weights(member).await
    }
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        self.inner.observe_work(member).await
    }
    async fn cancel_work(
        &self,
        member: &MemberRef,
        request: &RequestRef,
        require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        self.inner.cancel_work(member, request, require_ack).await
    }
    async fn idle_before_signal(
        &self,
        member: &MemberRef,
    ) -> Option<capyctl_adapters::traits::EngineWork> {
        self.inner.idle_before_signal(member).await
    }
    async fn engine_quiescent(&self, member: &MemberRef, after_ms: i64) -> bool {
        self.inner.engine_quiescent(member, after_ms).await
    }
}

#[cfg(test)]
mod tests;
