//! SPEC §13.2 (W13, G08): what the controller does when an owned engine exits.
//!
//! An engine that dies while Ready is learned of from evidence, not from the
//! next failed request: a remote host reports a `MemberExit` on its
//! authenticated session, and the embedded (standalone) role watches the
//! processes its own launcher started. Either way the controller
//!
//! 1. validates the exit against exactly the Ready launch it names (this
//!    session, current generation, the recorded group, the host that serves
//!    it) and closes that instance's dispatch, journaling `engine_exited`, in
//!    one store transaction. The router then skips the instance as
//!    `engine_exited`; other instances keep serving;
//! 2. settles it with an ordinary stop of that instance under the principal
//!    [`EXIT_PRINCIPAL`]. That is the existing verified cleanup: the host (or
//!    the local launcher) terminates whatever of the recorded group is still
//!    alive, and the reservation, endpoint and request leases are released only
//!    on evidence that the whole recorded group is gone. A partial group that
//!    stays alive keeps everything charged and the cleanup uncertain.
//!
//! Status reads `reconciling`, then `stopping`, then `failed` once settled.
//! Nothing restarts the engine here: the deployment stays eligible for
//! on-demand activation, so the next request relaunches it under the usual
//! policy (owner decision Q5). An exit that names no current Ready launch
//! (another generation, a stopping or settled launch) changes nothing.
use crate::coordinator::CoordinatorCommands;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_protocol::reports::{ExitStatus, MemberExit};
use capyctl_store::ordinary_lifecycle::engine_exit::{EngineExit, ExitSource, ExitedLaunch};
use std::sync::Arc;
use std::time::Duration;

/// The principal the settlement stop is accepted under. Status reads an
/// instance stopped by it as `failed` rather than `stopped`.
pub const EXIT_PRINCIPAL: &str = "system:engine_exit";
/// How often the embedded role checks its Ready engines' processes.
pub const LOCAL_SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// The Stop window when the revision records none.
const DEFAULT_STOP_WINDOW_MS: i64 = 60_000;

/// What handling one exit observation did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitHandled {
    /// It named no current Ready launch; nothing changed.
    Stale,
    /// Dispatch closed and the settlement stop was accepted (its operation).
    Settling { operation_id: Option<String> },
    /// Dispatch closed, but the settlement stop could not be accepted now; the
    /// next observation of the same exit retries it. Everything stays charged.
    Suspended { reason: String },
}

#[derive(Clone)]
pub struct EngineExits {
    commands: CoordinatorCommands,
}

impl EngineExits {
    pub fn new(commands: CoordinatorCommands) -> Self {
        Self { commands }
    }

    /// A `MemberExit` an authenticated host session reported. Blocking store
    /// work; call it off the async threads.
    pub fn remote(&self, host: &str, exit: &MemberExit) -> ExitHandled {
        self.settle(
            ExitSource::Host(host),
            EngineExit {
                deployment_id: exit.deployment_id.clone(),
                generation: exit.generation,
                step_id: exit.owned_handle.clone(),
                process: exit.process.clone(),
                status: describe(exit.status),
                observed_at_ms: exit.observed_at_ms,
            },
        )
    }

    /// Watch the embedded role's Ready engines until the task is aborted.
    pub fn spawn_local(self) -> tokio::task::JoinHandle<()> {
        self.spawn_local_until(crate::supervised::never())
    }

    /// Watch the embedded role's Ready engines until `cancel` reads true (or
    /// the task is aborted). Cancel is honoured only between passes, so a
    /// shutdown that cancels and then awaits the task never leaves a blocking
    /// pass still settling an exit against the coordinator's owned state after
    /// the role has returned (ADR 0015 invariant 6: no effect outlives the
    /// worker).
    pub fn spawn_local_until(
        self,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let exits = Arc::new(self);
        tokio::spawn(async move {
            loop {
                let pass = exits.clone();
                if tokio::task::spawn_blocking(move || pass.local_pass())
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => return,
                    _ = tokio::time::sleep(LOCAL_SCAN_INTERVAL) => {}
                }
            }
        })
    }

    /// One pass over this session's Ready embedded launches. Only processes
    /// recorded against this boot are judged (`exit_observed`): an engine this
    /// role did not start on this boot is never declared exited from here. Of
    /// those, only the engine's own (`api`, `worker-N`), never a helper.
    pub fn local_pass(&self) -> Vec<ExitHandled> {
        let launches = {
            let Ok(owner) = self.commands.owner_for_read() else {
                return Vec::new();
            };
            match owner.store().local_ready_launches(owner.session()) {
                Ok(launches) => launches,
                Err(_) => return Vec::new(),
            }
        };
        let mut handled = Vec::new();
        for launch in launches {
            // ADR 0027: only the engine's own processes are judged. A helper
            // (an idle compile worker) exits on its own; cleanup still ends it.
            let mut group = capyctl_domain::completion::engine_members(&launch.identities);
            group.sort_by_key(|p| (p.role != "api", p.role.clone(), p.pid));
            let Some(process) = group
                .into_iter()
                .find(capyctl_launchers::process_absence::exit_observed)
            else {
                continue;
            };
            let status = local_status(&process);
            handled.push(self.settle(
                ExitSource::Embedded,
                EngineExit {
                    deployment_id: launch.fence.deployment_id.clone(),
                    generation: launch.fence.generation,
                    step_id: launch.step_id.clone(),
                    process,
                    status,
                    observed_at_ms: capyctl_protocol::now_unix_ms(),
                },
            ));
        }
        handled
    }

    fn settle(&self, source: ExitSource<'_>, exit: EngineExit) -> ExitHandled {
        let recorded = {
            let Ok(owner) = self.commands.owner_for_read() else {
                return ExitHandled::Suspended {
                    reason: "coordinator state unavailable".into(),
                };
            };
            owner
                .store()
                .record_engine_exit(owner.session(), source, &exit)
        };
        let launch = match recorded {
            Ok(Some(launch)) => launch,
            Ok(None) => return ExitHandled::Stale,
            Err(error) => {
                return ExitHandled::Suspended {
                    reason: format!("the exit could not be recorded: {error}"),
                }
            }
        };
        if launch.first {
            capyctl_domain::role_log::notice(
                capyctl_domain::role_log::Level::Warning,
                &format!(
                    "deployment {} instance {}: engine exited ({}); dispatch closed, settling \
                 with verified cleanup",
                    launch.fence.deployment_id, launch.instance_index, exit.status
                ),
            );
        }
        match self.stop(&launch) {
            Ok(operation_id) => ExitHandled::Settling { operation_id },
            Err(reason) => {
                // Retried on every later observation of the same exit; said once.
                if launch.first {
                    capyctl_domain::role_log::notice(
                        capyctl_domain::role_log::Level::Warning,
                        &format!(
                            "deployment {} instance {}: the cleanup of the exited engine was not \
                         accepted yet ({reason}); dispatch stays closed and accounting charged",
                            launch.fence.deployment_id, launch.instance_index
                        ),
                    );
                }
                ExitHandled::Suspended { reason }
            }
        }
    }

    /// SPEC §6.3: an ordinary stop of exactly the exited instance, so the
    /// deployment stays eligible for on-demand activation. Keyed by the
    /// binding, so a repeated exit report replays the same stop.
    fn stop(&self, launch: &ExitedLaunch) -> Result<Option<String>, String> {
        let deployment = launch.fence.deployment_id.as_str();
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the deployment is gone".to_owned())?;
        let window = self
            .commands
            .read(|store| store.lifecycle_windows(deployment))
            .map_err(|error| error.to_string())?
            .map_or(DEFAULT_STOP_WINDOW_MS, |windows| windows.stop_ms);
        let deadline = self
            .commands
            .now_ms()
            .map_err(|error| error.to_string())?
            .checked_add(window)
            .ok_or_else(|| "clock overflow".to_owned())?;
        let key = format!("engine-exit:{}", launch.binding_id);
        self.commands
            .stop_instance(
                EXIT_PRINCIPAL,
                deployment,
                launch.instance_index,
                revision,
                &key,
                deadline,
            )
            .map(|receipt| receipt.map(|receipt| receipt.operation_id().to_owned()))
            .map_err(|error| error.to_string())
    }
}

/// A closed phrase for how a process ended.
pub fn describe(status: ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("exit code {code}"),
        ExitStatus::Signal(signal) => format!("signal {signal}"),
        ExitStatus::Unobserved => "exit status unobserved".into(),
    }
}

/// How an embedded engine process ended, when this role's launcher reaped it.
fn local_status(process: &ProcessIdentity) -> String {
    describe(match capyctl_launchers::reaped::status_of(process) {
        Some(capyctl_launchers::reaped::ReapedStatus::Code(code)) => ExitStatus::Code(code),
        Some(capyctl_launchers::reaped::ReapedStatus::Signal(signal)) => ExitStatus::Signal(signal),
        None => ExitStatus::Unobserved,
    })
}
