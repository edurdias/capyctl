//! SPEC §13.3 / T21: reading the bounded, redacted end of one instance's
//! engine log for the management surface.
//!
//! The instance's current launch is the retained (not released) binding of
//! its generation; its incarnation names the log. A server asks the
//! instance's host with the read-only `EngineLogTail` action (sent only to a
//! host that declared `engine_log_tail`, ADR 0017); a standalone role reads
//! its embedded host's log in-process with the same bounded reader. Every
//! failure is a closed category: no answer carries a path or native error
//! text, and a raw development log (`--debug-engine-logs`) is never served.
use crate::agent_sessions::{gate_refusal, AgentSessions};
use capyctl_adapters::engine_log::TailError;
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::{
    capabilities,
    execution::{EngineLogTailPlan, MemberAction, MemberCommand, MAX_ENGINE_LOG_TAIL_BYTES},
    pb,
};
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// The bound on one remote read: the host answers at once from a file.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(15);

/// The bounded, redacted end of an engine log: whole lines only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    /// Older output exists before `text`.
    pub truncated: bool,
}

/// Why no tail was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailFailure {
    /// The launch wrote no log (or no longer has one).
    Missing,
    /// SPEC §13.3: written under `--debug-engine-logs`; never served.
    Raw,
    /// The host could not read it; the reason stays on the host.
    Unreadable,
    /// ADR 0017: `host_capability_missing:<name>` (or another typed gate
    /// refusal); nothing was sent.
    CapabilityMissing(String),
    /// The instance's host has no live control session.
    HostOffline,
    /// The host did not answer within [`REQUEST_DEADLINE`].
    DeadlineExceeded,
    /// The owned state could not be read.
    Unavailable,
}

/// The launch an instance runs now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchScope {
    pub host_id: String,
    pub generation: i64,
    pub revision: i64,
    pub incarnation: String,
}

/// Why an instance names no launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// No such deployment or instance.
    NotFound,
    /// The instance holds no retained launch now.
    NotRunning,
    /// The owned state could not be read.
    Unavailable,
}

/// The deployment's instance indexes as status shows them, by index.
pub fn instance_indexes(
    store: &capyctl_store::Store,
    deployment_id: &str,
) -> Result<Vec<u32>, ScopeError> {
    let rows = store
        .deployment_instances(deployment_id)
        .map_err(|_| ScopeError::Unavailable)?;
    if rows.is_empty() {
        return Err(ScopeError::NotFound);
    }
    Ok(rows.into_iter().map(|row| row.index).collect())
}

/// The launch instance `index` of the deployment runs now: its host and the
/// retained binding of exactly its current generation.
pub fn launch_scope(
    store: &capyctl_store::Store,
    deployment_id: &str,
    index: u32,
) -> Result<LaunchScope, ScopeError> {
    let row = store
        .deployment_instances(deployment_id)
        .map_err(|_| ScopeError::Unavailable)?
        .into_iter()
        .find(|row| row.index == index)
        .ok_or(ScopeError::NotFound)?;
    let (Some(host_id), Some(generation)) = (row.host_id, row.generation) else {
        return Err(ScopeError::NotRunning);
    };
    let (binding, _) = store
        .serving_binding_at(deployment_id, generation)
        .map_err(|_| ScopeError::Unavailable)?
        .ok_or(ScopeError::NotRunning)?;
    // ADR 0015: the binding realizes exactly this instance.
    let lane = store
        .binding_lane(&binding.id)
        .map_err(|_| ScopeError::Unavailable)?;
    if lane != Some((deployment_id.to_owned(), index))
        || !capyctl_protocol::execution::is_incarnation(&binding.incarnation)
    {
        return Err(ScopeError::NotRunning);
    }
    Ok(LaunchScope {
        host_id,
        generation,
        revision: binding.revision,
        incarnation: binding.incarnation,
    })
}

/// The `EngineLogTail` command for one instance's launch. It names no
/// profile: the identity's profile field carries the read's own tag.
pub fn tail_command(
    controller_id: &str,
    deployment_id: &str,
    index: u32,
    scope: &LaunchScope,
    max_bytes: u32,
    deadline_ms: i64,
) -> MemberCommand {
    let id = ulid::Ulid::new().to_string();
    let mut command = MemberCommand {
        identity: CommandIdentity {
            controller_id: controller_id.into(),
            member: MemberKey {
                host_id: scope.host_id.clone(),
                member_id: "head".into(),
            },
            deployment_id: deployment_id.into(),
            operation_id: id.clone(),
            command_id: id.clone(),
            step_id: id,
            generation: scope.generation,
            revision: scope.revision,
            deadline_ms,
            payload_digest: [0; 32],
            expected_state: "engine_log".into(),
            profile_fingerprint: "engine_log".into(),
            instance_index: index,
        },
        action: MemberAction::EngineLogTail(EngineLogTailPlan {
            incarnation: scope.incarnation.clone(),
            max_bytes: max_bytes.clamp(1, MAX_ENGINE_LOG_TAIL_BYTES),
        }),
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// What an EngineLogTail result says, once `validate_result` bound its
/// shape (and its text's length) to the plan.
pub fn tail_from(result: &pb::MemberExecutionResult) -> Result<Tail, TailFailure> {
    let evidence = result.engine_log.as_ref().ok_or(TailFailure::Unreadable)?;
    match evidence.state.as_str() {
        "served" => Ok(Tail {
            text: evidence.text.clone(),
            truncated: evidence.truncated,
        }),
        "missing" => Err(TailFailure::Missing),
        "raw" => Err(TailFailure::Raw),
        _ => Err(TailFailure::Unreadable),
    }
}

/// Ask the instance's host for the tail through its control session.
/// ADR 0017: a host that did not declare `engine_log_tail` is refused,
/// typed, and sent nothing; an offline host is answered at once.
pub async fn remote_tail(
    sessions: &AgentSessions,
    controller_id: &str,
    deployment_id: &str,
    index: u32,
    scope: &LaunchScope,
    max_bytes: u32,
) -> Result<Tail, TailFailure> {
    if !sessions
        .inspect(&scope.host_id)
        .is_some_and(|view| view.online && view.reconciled)
    {
        return Err(TailFailure::HostOffline);
    }
    sessions
        .preflight(&scope.host_id, &[capabilities::ENGINE_LOG_TAIL], false)
        .map_err(TailFailure::CapabilityMissing)?;
    let command = tail_command(
        controller_id,
        deployment_id,
        index,
        scope,
        max_bytes,
        capyctl_protocol::now_unix_ms() + REQUEST_DEADLINE.as_millis() as i64,
    );
    let result = sessions
        .execute(command)
        .await
        .map_err(|status| match gate_refusal(&status) {
            Some(reason) => TailFailure::CapabilityMissing(reason.to_owned()),
            None if status.code() == tonic::Code::DeadlineExceeded => TailFailure::DeadlineExceeded,
            None => TailFailure::HostOffline,
        })?;
    tail_from(&result)
}

/// Where a standalone role's embedded host writes a launch's engine log:
/// `<log dir>/<deployment id>/<incarnation>.log`. The one derivation the
/// launch and the tail use.
pub fn standalone_log_path(log_dir: &Path, deployment_id: &str, incarnation: &str) -> PathBuf {
    log_dir
        .join(deployment_id)
        .join(format!("{incarnation}.log"))
}

/// [`standalone_log_path`] for a read named from outside: `None` when either
/// name is not one plain file-name component.
pub fn standalone_log(log_dir: &Path, deployment_id: &str, incarnation: &str) -> Option<PathBuf> {
    let plain = |name: &str| {
        let mut components = Path::new(name).components();
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
    };
    (plain(deployment_id) && capyctl_protocol::execution::is_incarnation(incarnation))
        .then(|| standalone_log_path(log_dir, deployment_id, incarnation))
}

/// Read a log on this machine with the host's bounded reader (a standalone
/// role's embedded host).
pub fn local_tail(log: &Path, max_bytes: usize) -> Result<Tail, TailFailure> {
    capyctl_adapters::engine_log::read_tail(log, max_bytes)
        .map(|tail| Tail {
            text: tail.text,
            truncated: tail.truncated,
        })
        .map_err(|failure| match failure {
            TailError::Missing => TailFailure::Missing,
            TailError::Raw => TailFailure::Raw,
            TailError::Unreadable(_) => TailFailure::Unreadable,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // T21: the standalone layout takes plain names only, never a path.
    #[test]
    fn the_standalone_log_is_named_by_plain_components_only() {
        let dir = Path::new("/state/logs");
        assert_eq!(
            standalone_log(dir, "dep-1", "01K00000000000000000000002"),
            Some(PathBuf::from(
                "/state/logs/dep-1/01K00000000000000000000002.log"
            ))
        );
        for (deployment, incarnation) in [
            ("..", "01K00000000000000000000002"),
            ("a/b", "01K00000000000000000000002"),
            ("/abs", "01K00000000000000000000002"),
            ("", "01K00000000000000000000002"),
            ("dep", "../x"),
            ("dep", ""),
        ] {
            assert_eq!(
                standalone_log(dir, deployment, incarnation),
                None,
                "{deployment} {incarnation}"
            );
        }
    }

    // T21: a result's closed state maps to the closed failure; text only
    // from `served`.
    #[test]
    fn a_result_maps_to_its_closed_category() {
        let result = |state: &str, text: &str| pb::MemberExecutionResult {
            engine_log: Some(pb::EngineLogTailEvidence {
                state: state.into(),
                text: text.into(),
                truncated: false,
            }),
            ..Default::default()
        };
        assert_eq!(
            tail_from(&result("served", "a\n")),
            Ok(Tail {
                text: "a\n".into(),
                truncated: false
            })
        );
        assert_eq!(tail_from(&result("missing", "")), Err(TailFailure::Missing));
        assert_eq!(tail_from(&result("raw", "")), Err(TailFailure::Raw));
        assert_eq!(
            tail_from(&result("unreadable", "")),
            Err(TailFailure::Unreadable)
        );
        assert_eq!(
            tail_from(&pb::MemberExecutionResult::default()),
            Err(TailFailure::Unreadable)
        );
    }
}
