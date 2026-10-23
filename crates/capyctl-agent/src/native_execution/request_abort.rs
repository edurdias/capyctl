//! ADR 0032 §3: abort one request on a retained SGLang launch by CapyCTL's
//! request ID. The host's agent posts the abort on the engine's own loopback
//! port with the launch's keys, through the SGLang adapter's `cancel_work`, as
//! it runs the wake canary (ADR 0028 §9): never through its ingress or the
//! router. The answer is never evidence that the request ended.
use super::NativeHostExecution;
use crate::session::SessionError;
use capyctl_adapters::traits::{CancellationOutcome, MemberRef, RequestRef};
use capyctl_config::{effective::EffectiveDeployment, engine_policy::Engine};
use capyctl_domain::launch::LaunchSettings;
use capyctl_protocol::{
    execution::{MemberCommand, ABORT_REQUEST_STATE},
    pb,
};
use std::time::Duration;

/// The longest the post may take. The SGLang adapter bounds its own request
/// at this too; this bound also covers building the request.
const ABORT_POST_BOUND: Duration = Duration::from_secs(3);
/// The post ends this far ahead of the command deadline, so the controller
/// receives the result rather than a transport timeout.
const ABORT_MARGIN_MS: i64 = 250;

/// ADR 0032 §1, ADR 0017: whether a launch resolved as `effective` takes
/// CapyCTL's request ID as `rid` and can be asked to abort it: SGLang, at
/// the version it is registered with and the tokenizer workers it was
/// approved with, as `capyctl_adapters::sglang::request_abort_supported`
/// decides. The ingress (`rid`) and the abort use this one answer.
pub(super) fn request_abort_capable(effective: &EffectiveDeployment) -> bool {
    match (&effective.profile.engine, &effective.engine_config) {
        (Engine::Sglang, LaunchSettings::Sglang(settings)) => {
            capyctl_adapters::sglang::request_abort_supported(
                &effective.profile.build_fingerprint,
                settings.tokenizer_workers,
            )
        }
        _ => false,
    }
}

/// Why an abort was not posted, as the result's closed refusal.
enum NotPosted {
    /// `launch_not_retained`.
    NotRetained,
    /// `request_abort_unsupported`.
    Unsupported,
}

impl NativeHostExecution {
    /// ADR 0032 §3: the AbortRequest action. Not journaled: it launches,
    /// releases and records nothing, it is sent once per cancelled request,
    /// and posting it again (a redelivery) is harmless, since SGLang answers
    /// an abort of a request it no longer runs with nothing done. Before
    /// anything is posted the journal binds it to the launch it names
    /// (`HostJournal::abort_target`: retained, serving, the same live engine,
    /// the command's member, deployment, profile and exact generation), and
    /// that launch's engine must have the capability. A refusal or a failed
    /// post is a result, never the end of the session.
    pub(super) async fn abort_request(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
        request_id: &str,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let id = &command.identity;
        if id.controller_id != self.controller_id
            || id.member.host_id != self.host_id
            || id.expected_state != ABORT_REQUEST_STATE
        {
            return Err(SessionError);
        }
        let answer = |state: &str, refused: &str| pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: state.into(),
            owned_handle: owned_handle.into(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            refused: refused.into(),
            ..Default::default()
        };
        let remaining = id.deadline_ms - ABORT_MARGIN_MS - capyctl_protocol::now_unix_ms();
        let Some(remaining) = u64::try_from(remaining).ok().filter(|ms| *ms > 0) else {
            return Ok(answer("completed", "deadline_passed"));
        };
        let (adapter, member) = match self.abort_adapter(command, owned_handle).await? {
            Ok(found) => found,
            Err(NotPosted::NotRetained) => return Ok(answer("completed", "launch_not_retained")),
            Err(NotPosted::Unsupported) => {
                return Ok(answer("completed", "request_abort_unsupported"))
            }
        };
        let bound = ABORT_POST_BOUND.min(Duration::from_millis(remaining));
        let posted = tokio::time::timeout(
            bound,
            adapter.cancel_work(
                &member,
                &RequestRef {
                    id: request_id.into(),
                },
                false,
            ),
        )
        .await;
        Ok(match posted {
            // The engine answered the abort. Whether the request ended is
            // the router's to learn, from the request's own final output.
            Ok(Ok(CancellationOutcome::Uncertain | CancellationOutcome::Acknowledged)) => {
                answer("completed", "")
            }
            // Posted, or perhaps not, with no answer from the engine.
            _ => answer("attempted", ""),
        })
    }

    /// The adapter an abort posts through, for the retained launch the
    /// command names, or why none may be built.
    async fn abort_adapter(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
    ) -> Result<Result<(Box<dyn super::NativeEngine>, MemberRef), NotPosted>, SessionError> {
        let journal = self.journal.clone();
        let checked = command.clone();
        if tokio::task::spawn_blocking(move || journal.abort_target(&checked))
            .await
            .map_err(|_| SessionError)?
            .is_err()
        {
            return Ok(Err(NotPosted::NotRetained));
        }
        let Ok(owned) = self.journal.retained_command(owned_handle) else {
            return Ok(Err(NotPosted::NotRetained));
        };
        let Some(plan) = owned.action.launch_plan() else {
            return Ok(Err(NotPosted::NotRetained));
        };
        // The version and settings the launch was approved with, never the
        // host's current profile set; unresolvable is not capable.
        let Ok(effective) = self.resolve_retained(&owned) else {
            return Ok(Err(NotPosted::Unsupported));
        };
        if !request_abort_capable(&effective) {
            return Ok(Err(NotPosted::Unsupported));
        }
        let Ok(scope) = self.scope(&owned) else {
            return Ok(Err(NotPosted::NotRetained));
        };
        let Ok(keys) = self.identities.load(&scope, owned.identity.payload_digest) else {
            return Ok(Err(NotPosted::NotRetained));
        };
        let Some(served) = effective.routes.first().cloned() else {
            return Ok(Err(NotPosted::Unsupported));
        };
        let Ok(adapter) = self.probe_adapter(&effective, plan, &keys, &served) else {
            return Ok(Err(NotPosted::Unsupported));
        };
        Ok(Ok((
            adapter,
            MemberRef {
                deployment_id: owned.identity.deployment_id.clone(),
                member_id: plan.binding_id.clone(),
            },
        )))
    }
}
