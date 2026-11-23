//! ADR 0037 §4: the host's own lifecycle steps (`capyctl.host.launch`,
//! `capyctl.host.wake`, `capyctl.host.park`, `capyctl.host.stop`,
//! `capyctl.host.verify_release`) as children of the server step whose
//! context the command's envelope carried (§6). Nothing is recorded when the
//! host's tracing is off or the envelope carried no context (a command
//! replayed after a restart, or sent by a server that sends none).

use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb,
};
use capyctl_trace::{Attr, Incoming, Name, Outcome, Parent, Span, SpanCx, Tracing};

/// One command's step tracing: the host's tracing and the server step that
/// is every host step's parent.
pub(super) struct StepTrace {
    tracing: Tracing,
    parent: Option<Incoming>,
}

impl StepTrace {
    /// Nothing is recorded.
    pub(super) fn none() -> Self {
        Self {
            tracing: Tracing::off(),
            parent: None,
        }
    }

    /// The envelope's `traceparent`, already checked well formed by the
    /// protocol; one that is not is ignored here as well (§2).
    pub(super) fn new(tracing: Tracing, trace_parent: Option<&str>) -> Self {
        let parent = trace_parent
            .filter(|_| tracing.is_on())
            .and_then(SpanCx::parse_traceparent)
            .map(|parent| Incoming {
                parent,
                tracestate: None,
            });
        Self { tracing, parent }
    }

    /// Start one host step under the server's step, with CapyCTL's names
    /// for the command's deployment, instance, host and generation (§5).
    pub(super) fn step(&self, name: Name, command: &MemberCommand) -> Step {
        let Some(parent) = &self.parent else {
            return Step(None);
        };
        let mut span = self.tracing.span(name, Parent::Remote(parent), &[]);
        if span.is_recording() {
            let identity = &command.identity;
            span.set(Attr::Deployment(identity.deployment_id.clone()));
            span.set(Attr::Instance(identity.instance_index.to_string()));
            span.set(Attr::Host(identity.member.host_id.clone()));
            span.set(Attr::Generation(identity.generation));
        }
        Step(Some(span))
    }
}

/// One host step's span. One dropped before it ended (an error that ends
/// the session, whose effect is then unknown) ends as `uncertain`.
pub(super) struct Step(Option<Span>);

impl Step {
    pub(super) fn end(&mut self, outcome: Outcome, code: Option<&str>) {
        if let Some(mut span) = self.0.take() {
            span.set(Attr::Outcome(outcome));
            if let Some(code) = code {
                span.fail(code);
            }
            span.end();
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        self.end(Outcome::Uncertain, None);
    }
}

/// The step a command records around its whole effect: a launch, a wake or
/// a park. A Terminate records its stop and its release check apart.
pub(super) fn whole_step(action: &MemberAction) -> Option<Name> {
    match action {
        MemberAction::LaunchSingle(_) | MemberAction::Launch { .. } => Some(Name::HostLaunch),
        MemberAction::Restore { .. } => Some(Name::HostWake),
        MemberAction::Park { .. } => Some(Name::HostPark),
        _ => None,
    }
}

/// ADR 0037 §5: how a launch, wake or park ended, from its result: refused
/// with its closed code; a park or wake by its residency evidence; a launch
/// completed only with a usable model, failed when its processes are gone,
/// and otherwise uncertain.
pub(super) fn whole_outcome(
    step: Name,
    result: &pb::MemberExecutionResult,
) -> (Outcome, Option<&str>) {
    if !result.refused.is_empty() {
        return (Outcome::Refused, Some(&result.refused));
    }
    let outcome = match step {
        Name::HostPark | Name::HostWake => {
            match result.residency.as_ref().map(|r| r.state.as_str()) {
                Some("parked" | "restored") => Outcome::Completed,
                Some("unchanged") => Outcome::Refused,
                _ => Outcome::Uncertain,
            }
        }
        _ if result.model_usable => Outcome::Completed,
        _ if !result.processes.is_empty()
            && result.processes.iter().all(|p| p.presence == "gone") =>
        {
            Outcome::Failed
        }
        _ => Outcome::Uncertain,
    };
    (outcome, None)
}
