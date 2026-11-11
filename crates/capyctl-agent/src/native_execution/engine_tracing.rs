//! ADR 0037 §§7, 11: a launch's engine tracing on this host, decided once
//! when it launches and frozen with it: journaled before its engine starts,
//! reported with its result, and bound to its ingress entry (`EngineAccess`).
use super::NativeHostExecution;
use capyctl_adapters::engine_tracing::{
    host_setting, EngineTracing, EngineTracingOff, TracingOutcome,
};
use capyctl_config::{effective::EffectiveDeployment, lmcache::Retention};

impl NativeHostExecution {
    /// ADR 0037 §§7, 11: the decision for a launch resolved as `effective`:
    /// zero retention is the deployment's own, or this host's in its approved
    /// document or the policy the launch resolved against (as ADR 0033
    /// §§9, 10 read it for LMCache); `group` for a multi-node group member.
    pub(super) fn engine_tracing(
        &self,
        effective: &EffectiveDeployment,
        group: bool,
    ) -> EngineTracing {
        let (_, retention) =
            capyctl_config::lmcache::host_settings(&self.profiles.accepted().config.document);
        let retention_zero = retention == Retention::Zero
            || effective.host.retention == Retention::Zero
            || effective.retention == Retention::Zero;
        crate::tracing_install::decide_launch(
            effective.profile.engine,
            &effective.profile.build_fingerprint,
            std::path::Path::new(&effective.profile.executable),
            retention_zero,
            group,
        )
    }

    /// ADR 0037 §7: the frozen decision of the launch `owned_handle`, as its
    /// journal holds it. A launch journaled before engine tracing, or whose
    /// record cannot be read, is untraced (the conservative answer: no trace
    /// context reaches its engine).
    pub(super) fn frozen_engine_trace(&self, owned_handle: &str) -> bool {
        self.journal
            .launch_engine_tracing(owned_handle)
            .ok()
            .flatten()
            .and_then(|state| TracingOutcome::parse(&state))
            .is_some_and(TracingOutcome::is_on)
    }

    /// ADR 0033 §7, ADR 0037 §7: the engine tracing a restart-parked launch's
    /// engine starts with again: on only when the launch was traced and this
    /// role still exports engine spans, to the endpoint it names now (a role
    /// restart may have changed it); otherwise the engine starts untraced.
    pub(super) fn restored_engine_tracing(&self, owned_handle: &str) -> EngineTracing {
        let host = host_setting();
        match (self.frozen_engine_trace(owned_handle), host.endpoint) {
            (true, Some(endpoint)) if host.engines_on => EngineTracing::On { endpoint },
            _ => EngineTracing::Off(EngineTracingOff::HostOff),
        }
    }
}
