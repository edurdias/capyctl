//! ADR 0028 §6: a group's weights on every host, and digest agreement.
//!
//! Every named host materializes the model source at once, through the same
//! per-host path a single-host launch uses (`model_sources`, whose host checks
//! free space before it downloads), and then measures its copy's checkpoint
//! digest (`checkpoint_digests`). Only when every host has answered does the
//! group decide: a host short of space, a host that failed, or digests that
//! differ refuse the group before anything is reserved or launched. Each
//! host's own model path is returned so activation can compare them and write
//! them into the group plan (§4).
//!
//! Nothing here reserves, launches or releases anything.
use crate::{
    checkpoint_digests::{self, MeasureError},
    group_activation::GroupHosts,
    model_sources,
    ownership::SharedCoordinatorState,
};
use capyctl_adapters::traits::RuntimeError;
use capyctl_config::model_source::{reason, ModelSource};
use std::{collections::BTreeMap, future::Future, sync::Arc};

/// The closed code of a group whose checkpoint digests differ (spec §16).
const GROUP_CHECKPOINT_MISMATCH: &str = "group_checkpoint_mismatch";
/// No answer: the host is offline or the command did not complete.
const UNAVAILABLE: &str = "unavailable";

/// One host's verified copy of the group's weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialized {
    /// The path this host loads the weights from (OD2: compared by activation).
    pub path: String,
    /// The checkpoint digest this host measured for its copy.
    pub digest: String,
    /// ADR 0014 §7: the weights it measured beside the digest.
    pub weights_bytes: i64,
    /// ADR 0014 amendment A16: the hybrid state slot read beside them.
    pub state_slot_bytes: Option<i64>,
}

/// Why one host has no verified copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFailure {
    /// The host's filesystem has no room for the download.
    InsufficientSpace,
    /// Any other failure, with the host's own closed reason code unchanged.
    Failed(String),
}

/// Why the group cannot launch on its weights.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupSourceError {
    #[error(
        "insufficient_space: not enough free disk for the model source on {}",
        .hosts.join(", ")
    )]
    Space { hosts: Vec<String> },
    #[error("{reason}: the model source could not be materialized on {host}")]
    Failed { host: String, reason: String },
    #[error(
        "group_checkpoint_mismatch: the hosts' checkpoint digests differ ({})",
        listed(.digests)
    )]
    Mismatch { digests: BTreeMap<String, String> },
}

fn listed(digests: &BTreeMap<String, String>) -> String {
    digests
        .iter()
        .map(|(host, digest)| format!("{host}: {digest}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl GroupSourceError {
    /// The closed code (spec §16); a host's failure keeps its own reason.
    pub fn code(&self) -> &str {
        match self {
            Self::Space { .. } => reason::INSUFFICIENT_SPACE,
            Self::Failed { reason, .. } => reason,
            Self::Mismatch { .. } => GROUP_CHECKPOINT_MISMATCH,
        }
    }
}

/// How one host is asked to materialize the group's weights and measure them.
pub trait SourceDriver: Sync {
    fn materialize(
        &self,
        host: &str,
        source: &ModelSource,
    ) -> impl Future<Output = Result<Materialized, SourceFailure>> + Send;
}

/// ADR 0028 §6 (owner decision 11): materialize `source` on every host in
/// parallel. Every host's outcome is collected before deciding, so no host is
/// left mid-request when another fails. Hosts short of space refuse the group
/// together (`insufficient_space`, naming each); otherwise the first failed
/// host in `hosts` order refuses it with its own reason. Dropping the returned
/// future drops every request in flight together; a download the host already
/// started keeps running there and is reported by the next attempt.
pub async fn materialize_on_all(
    hosts: &[String],
    source: &ModelSource,
    driver: &impl SourceDriver,
) -> Result<BTreeMap<String, Materialized>, GroupSourceError> {
    let outcomes = futures::future::join_all(
        hosts
            .iter()
            .map(|host| async move { (host, driver.materialize(host, source).await) }),
    )
    .await;
    let mut done = BTreeMap::new();
    let mut short = Vec::new();
    let mut failed = None;
    for (host, outcome) in outcomes {
        match outcome {
            Ok(materialized) => {
                done.insert(host.clone(), materialized);
            }
            Err(SourceFailure::InsufficientSpace) => short.push(host.clone()),
            Err(SourceFailure::Failed(reason)) => {
                failed.get_or_insert_with(|| GroupSourceError::Failed {
                    host: host.clone(),
                    reason,
                });
            }
        }
    }
    if !short.is_empty() {
        return Err(GroupSourceError::Space { hosts: short });
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(done),
    }
}

/// ADR 0028 §6: the one digest every host measured. Digests that differ, or
/// no digest at all, refuse the group (`group_checkpoint_mismatch`, naming
/// every host with its digest). Digests are compared as recorded; their
/// format was checked when each host's measurement was taken and recorded.
pub fn agree_digests(per_host: &BTreeMap<String, String>) -> Result<String, GroupSourceError> {
    let mut digests = per_host.values();
    match digests.next() {
        Some(first) if digests.all(|digest| digest == first) => Ok(first.clone()),
        _ => Err(GroupSourceError::Mismatch {
            digests: per_host.clone(),
        }),
    }
}

/// What a single-host materialization refusal means for a group member.
fn failure_from(error: RuntimeError) -> SourceFailure {
    match error {
        RuntimeError::Refused(code)
            if code.strip_prefix("model_source:") == Some(reason::INSUFFICIENT_SPACE) =>
        {
            SourceFailure::InsufficientSpace
        }
        RuntimeError::Refused(code) => SourceFailure::Failed(code),
        _ => SourceFailure::Failed(UNAVAILABLE.into()),
    }
}

/// One member's part of a group's materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSource {
    /// The member on this host (`capyctl_domain::group::member_id`).
    pub member_id: String,
    /// The host-local deployment document the host materializes from.
    pub deployment_config: String,
    pub host_policy_fingerprint: String,
    pub profile_fingerprint: String,
    /// The path this host's copy resolves to.
    pub model_path: String,
}

/// The production driver: each host is asked through its authenticated
/// session, with the same `MaterializeSource` and `DigestCheckpoint` requests
/// a single-host launch sends, addressed to the host's own member. Each
/// host's digest is recorded under that host (`record_digest`).
///
/// Each host materializes what its own document declares; `source` is the
/// group's declaration, the same in every member's document.
pub struct RemoteGroupSources {
    pub owner: SharedCoordinatorState,
    /// ADR 0028 §8: the member hosts' sessions.
    pub hosts: Arc<dyn GroupHosts>,
    pub controller_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub deadline_ms: i64,
    /// Every member, by host id.
    pub members: BTreeMap<String, MemberSource>,
}

impl SourceDriver for RemoteGroupSources {
    async fn materialize(
        &self,
        host: &str,
        _source: &ModelSource,
    ) -> Result<Materialized, SourceFailure> {
        let member = self
            .members
            .get(host)
            .ok_or_else(|| SourceFailure::Failed("unauthorized".into()))?;
        // ADR 0017: a host without the digest action is refused before it is
        // asked anything, so no download starts for a member that could
        // never be measured (the single-host launch's preflight order).
        self.hosts
            .preflight(host, &[capyctl_protocol::capabilities::CHECKPOINT_DIGEST])
            .map_err(SourceFailure::Failed)?;
        model_sources::ensure_materialized_with(
            &self.owner,
            self.hosts
                .supports(host, capyctl_protocol::capabilities::MODEL_SOURCES),
            |command| self.hosts.execute(command),
            &self.controller_id,
            host,
            &member.member_id,
            &self.deployment_id,
            self.revision,
            self.generation,
            &member.profile_fingerprint,
            &member.deployment_config,
            &member.host_policy_fingerprint,
            self.deadline_ms,
        )
        .await
        .map_err(failure_from)?;
        let command = checkpoint_digests::digest_command(
            &self.controller_id,
            host,
            &member.member_id,
            &self.deployment_id,
            self.revision,
            self.generation,
            &member.profile_fingerprint,
            member.deployment_config.clone(),
            member.host_policy_fingerprint.clone(),
            (capyctl_protocol::now_unix_ms()
                + checkpoint_digests::DIGEST_DEADLINE.as_millis() as i64)
                .min(self.deadline_ms),
        );
        let unavailable = || SourceFailure::Failed(UNAVAILABLE.into());
        let result = self
            .hosts
            .execute(command)
            .await
            .map_err(|_| unavailable())?;
        let measured = checkpoint_digests::measured_from(&result).map_err(|error| match error {
            MeasureError::Refused(code) => SourceFailure::Failed(code),
            MeasureError::Unavailable => unavailable(),
        })?;
        // ADR 0028 §6: the store keeps one digest per host.
        self.owner
            .lock()
            .map_err(|_| unavailable())?
            .store()
            .record_digest(&self.deployment_id, self.revision, host, &measured.digest)
            .map_err(|_| unavailable())?;
        Ok(Materialized {
            path: member.model_path.clone(),
            digest: measured.digest,
            weights_bytes: measured.weights_bytes,
            state_slot_bytes: measured.state_slot_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[derive(Default, Clone)]
    struct FakeSourceDriver {
        answers: BTreeMap<String, Result<Materialized, SourceFailure>>,
        in_flight: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
    }

    impl FakeSourceDriver {
        fn new() -> Self {
            Self::default()
        }
        fn host(mut self, host: &str, answer: Result<Materialized, SourceFailure>) -> Self {
            self.answers.insert(host.into(), answer);
            self
        }
        fn max_in_flight(&self) -> usize {
            self.max.load(Ordering::SeqCst)
        }
    }

    impl SourceDriver for FakeSourceDriver {
        async fn materialize(
            &self,
            host: &str,
            _source: &ModelSource,
        ) -> Result<Materialized, SourceFailure> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(now, Ordering::SeqCst);
            // A download takes time: yield so a sequential caller is seen.
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.answers
                .get(host)
                .cloned()
                .unwrap_or(Err(SourceFailure::Failed("unavailable".into())))
        }
    }

    fn mat(path: &str, digest: &str) -> Materialized {
        Materialized {
            path: path.into(),
            digest: digest.into(),
            weights_bytes: 1,
            state_slot_bytes: None,
        }
    }

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn hf_source() -> ModelSource {
        ModelSource::HuggingFace {
            repo: "org/toy".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
            files: Vec::new(),
            token_ref: None,
        }
    }

    // T07: every host materializes concurrently; hosts short of space fail the group before launch.
    #[tokio::test]
    async fn materialization_is_parallel_and_space_is_checked() {
        let driver = FakeSourceDriver::new()
            .host("host-a", Ok(mat("/m", "sha256:c")))
            .host("host-b", Err(SourceFailure::InsufficientSpace))
            .host("host-c", Err(SourceFailure::InsufficientSpace));
        let err = materialize_on_all(
            &hosts(&["host-a", "host-b", "host-c"]),
            &hf_source(),
            &driver,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "insufficient_space");
        assert!(err.to_string().contains("host-b") && err.to_string().contains("host-c"));
        assert_eq!(driver.max_in_flight(), 3);
    }

    // T14: digests must agree across hosts; a mismatch names every host.
    #[test]
    fn digests_must_agree() {
        let same = BTreeMap::from([
            ("host-a".into(), "sha256:c".into()),
            ("host-b".into(), "sha256:c".into()),
        ]);
        assert_eq!(agree_digests(&same).unwrap(), "sha256:c");
        let diff = BTreeMap::from([
            ("host-a".into(), "sha256:c".into()),
            ("host-b".into(), "sha256:d".into()),
        ]);
        let err = agree_digests(&diff).unwrap_err();
        assert_eq!(err.code(), "group_checkpoint_mismatch");
        assert!(err.to_string().contains("host-a") && err.to_string().contains("host-b"));
    }

    // T07: with every host verified, each host's own path and digest come back
    // (OD2: activation compares the paths).
    #[tokio::test]
    async fn every_host_reports_its_own_path_and_digest() {
        let driver = FakeSourceDriver::new()
            .host("host-a", Ok(mat("/a/m", "sha256:c")))
            .host("host-b", Ok(mat("/b/m", "sha256:c")));
        let done = materialize_on_all(&hosts(&["host-a", "host-b"]), &hf_source(), &driver)
            .await
            .unwrap();
        assert_eq!(
            done,
            BTreeMap::from([
                ("host-a".to_string(), mat("/a/m", "sha256:c")),
                ("host-b".to_string(), mat("/b/m", "sha256:c")),
            ])
        );
        assert_eq!(driver.max_in_flight(), 2);
    }

    // T07: a failed host refuses the group with its own reason, named; space
    // shortages are reported first, every short host named.
    #[tokio::test]
    async fn a_failed_host_keeps_its_reason() {
        let driver = FakeSourceDriver::new()
            .host("host-a", Ok(mat("/m", "sha256:c")))
            .host(
                "host-b",
                Err(SourceFailure::Failed("model_source:not_found".into())),
            );
        let err = materialize_on_all(&hosts(&["host-a", "host-b"]), &hf_source(), &driver)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            GroupSourceError::Failed {
                host: "host-b".into(),
                reason: "model_source:not_found".into()
            }
        );
        assert_eq!(err.code(), "model_source:not_found");
        assert!(err.to_string().contains("host-b"));

        let driver = driver
            .host("host-a", Err(SourceFailure::InsufficientSpace))
            .host("host-c", Err(SourceFailure::Failed("network".into())));
        let err = materialize_on_all(
            &hosts(&["host-a", "host-b", "host-c"]),
            &hf_source(),
            &driver,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            GroupSourceError::Space {
                hosts: hosts(&["host-a"])
            }
        );
    }

    // T07: the single-host refusals map onto a member's failure unchanged.
    #[test]
    fn single_host_refusals_become_member_failures() {
        assert_eq!(
            failure_from(RuntimeError::Refused(
                "model_source:insufficient_space".into()
            )),
            SourceFailure::InsufficientSpace
        );
        assert_eq!(
            failure_from(RuntimeError::Refused("model_source_pending".into())),
            SourceFailure::Failed("model_source_pending".into())
        );
        assert_eq!(
            failure_from(RuntimeError::Uncertain("no answer".into())),
            SourceFailure::Failed("unavailable".into())
        );
    }

    // T14: no digest at all is no agreement.
    #[test]
    fn no_digest_is_no_agreement() {
        assert_eq!(
            agree_digests(&BTreeMap::new()).unwrap_err().code(),
            "group_checkpoint_mismatch"
        );
    }

    // T39: a single-host request still names `head`; a group member's names
    // its own member, and nothing else differs.
    #[test]
    fn requests_name_the_member_they_are_for() {
        let source = |member: &str| {
            model_sources::source_command(
                "controller",
                "host-b",
                member,
                "dep",
                1,
                1,
                "fp",
                capyctl_protocol::execution::MaterializeSourcePlan {
                    deployment_config: String::new(),
                    host_policy_fingerprint: String::new(),
                    source_key: String::new(),
                },
                0,
            )
        };
        let digest = |member: &str| {
            checkpoint_digests::digest_command(
                "controller",
                "host-b",
                member,
                "dep",
                1,
                1,
                "fp",
                String::new(),
                String::new(),
                0,
            )
        };
        for command in [source("head"), digest("head")] {
            assert_eq!(command.identity.member.member_id, "head");
            assert_eq!(command.identity.member.host_id, "host-b");
        }
        for command in [source("worker-1"), digest("worker-1")] {
            assert_eq!(command.identity.member.member_id, "worker-1");
            assert_eq!(command.identity.payload_digest, command.canonical_digest());
        }
    }
}
