# Native Launch, vLLM First (S1) Implementation Plan

**Goal:** The coordinator launches, serves, stops and fails a real vLLM engine on `host-a` through its own state machine, proven by eleven live scenarios.

**Architecture:** Director and builder. The coordinator directs: it resolves the vLLM builder from the frozen profile, hands it process tools, calls Initialize, records identities and facts, and on failure terminates, proves gone and releases with evidence. The vLLM adapter builds: it renders the command, spawns through a durable gated launcher, waits for readiness, probes, and enumerates the process group. Every engine handle is rebuildable from stored facts (identities, endpoint, served name, launch settings, encrypted engine key) so slice S1r can re-attach after a restart. The Fake engine leaves the product for a test-only crate.

**Tech Stack:** Rust workspace; `rusqlite` with forward-only migrations; `tokio`; `nix` for process groups and signals; `chacha20poly1305` (new) for the engine key at rest; `axum` for test HTTP stubs; vLLM 0.29.0 on `host-a` in `~/mllm-vllm-venv2`.

> Retired 2026-09-23: `crates/mllm-cli/tests/live_vllm.rs` and `scripts/live/run-on-spark.sh`,
> which Task 15 creates, are deleted by owner decision (2026-09-22). Their scenarios are
> matrix rows M38 and M73–M75 driven through the shipped CLI and roles; see
> `docs/plans/2026-09-22-two-host-engine-matrix.md`, Tier 8. This plan is a
> historical record.

**Spec:** `docs/specs/2026-09-17-native-launch-vllm-design.md`. Read it first. Its §11 records the owner's decisions; where this plan and the spec disagree, the spec wins.

## Global Constraints

- Only host `host-a` is authorized for live work. Never access `host-b`. Only Task 15 touches the host, through `scripts/live/run-on-spark.sh`, which refuses any other host name.
- Do not change engine environments, drivers, or reboot hosts: nothing installed on the host is modified. mllm's own `runtime/` directory in this repository is in scope, and Task 7a creates `runtime/mllm_vllm_guard.py` there.
- Excluded files, never read, edited, formatted, tested or staged: `crates/mllm-cli/tests/live_interactive.rs` and a local Task 2 implementation report. `AGENTS.md` is untracked and not this plan's.
- One status document: `docs/runbooks/f2-current-status.md`. Hardware evidence goes in `docs/runbooks/spark-live-f2.md` (Task 15 creates it). No progress or continuation files under `docs/`.
- Prose in documents, doc comments and commit messages is normal English. Cite the governing requirement inline, e.g. `// SPEC §13.3: local-only is not unauthenticated`. Tag tests with acceptance-matrix identifiers as a comment line directly above the test attribute, e.g. `// T10`.
- Never release a reservation, advance an epoch, or replay a dispatch without verified evidence. The `Uncertain` pause in the worker is preserved for every state that cannot be proven.
- Migrations are forward-only. Never edit `SCHEMA_V1` through `SCHEMA_V13`. This plan adds `SCHEMA_V14`.
- Verification per task is scoped to the crates touched, with `--offline` where the lockfile allows (Task 4 adds a dependency and needs the network once). The core suite runs in Task 16. CPU and Fake-engine tests are a pre-check; they never establish that a native recipe works.
- The Fake engine must not be compilable into a release binary (spec §8). After Task 13, no product crate depends on `mllm-testkit`.
- The engine key never appears in argv, logs, journals, fingerprints or management output (SPEC §13.3, §8.2).
- Commits end with the attribution lines the session provides.

## Order of work, and why

The spec (§8) puts the vLLM path first and the Fake extraction after it, so the first native-launch evidence comes as early as possible. Tasks 1 to 12 build the path against the existing Fake fixtures. Task 13 extracts the Fake. Task 14 rewires standalone. Task 15 is the live run. Task 16 records.

## File structure

New files:
- `crates/mllm-launchers/src/owned_launch.rs` — `DurableProcessLaunch`, the one implementation of `OwnedProcessLaunch`.
- `crates/mllm-adapters/src/policy.rs` — `ParkPolicy`, moved out of the Fake module.
- `crates/mllm-adapters/src/vllm/initialize.rs` — the vLLM Initialize step.
- `crates/mllm-store/src/secrets.rs` — identity key file and the encrypted engine-key table.
- `crates/mllm-store/src/ordinary_lifecycle/failed_launch.rs` — `release_failed_launch`.
- `crates/mllm-controller/src/coordinator/native_failure.rs` — terminate, prove, release, journal, close.
- `crates/mllm-router/src/forwarders.rs` — per-deployment forwarder source.
- `crates/mllm-testkit/` — the Fake engine, fake launcher, lifecycle simulation and shared fixtures (Task 13).
- `crates/mllm-cli/tests/live_vllm.rs`, `scripts/live/run-on-spark.sh`, `docs/runbooks/spark-live-f2.md`.

Modified, by responsibility: `mllm-domain/src/completion.rs` (Presence, identity rules); `mllm-adapters/src/traits.rs` (the tool trait), `vllm/args.rs`, `vllm/adapter.rs`, `resolve.rs`; `mllm-launchers/src/{durable.rs, group_observation.rs, process_absence.rs, lib.rs}`; `mllm-store/src/{schema.rs, migrations.rs, lifecycle.rs, lifecycle/completion.rs, ordinary_lifecycle.rs}`; `mllm-config/src/{schema.rs, effective.rs, effective/core.rs, engine_policy.rs}`; `mllm-controller/src/{engine_bindings.rs, coordinator/worker.rs, coordinator_port.rs, port.rs}`; `mllm-router/src/{lib.rs, chat.rs}`; `mllm-cli/src/{roles.rs, standalone_config.rs}`.

---

### Task 1: `Presence` in the domain, `OwnedProcessLaunch` in the adapters, `ParkPolicy` out of the Fake

Types only; no behavior. Makes the seams every later task plugs into.

**Files:**
- Modify: `crates/mllm-domain/src/completion.rs` (add `Presence`)
- Modify: `crates/mllm-launchers/src/process_absence.rs` (delete its `Presence`, import the domain's)
- Modify: `crates/mllm-adapters/src/traits.rs` (add the trait)
- Create: `crates/mllm-adapters/src/policy.rs`; Modify: `crates/mllm-adapters/src/lib.rs`, `crates/mllm-adapters/src/fake/engine.rs`, `crates/mllm-adapters/src/fake/mod.rs`, every `ParkPolicy` importer (`grep -rn "fake::ParkPolicy\|fake::{.*ParkPolicy" crates`)

**Interfaces:**
- Produces: `mllm_domain::completion::Presence { Alive, Gone, Unknown }`; `mllm_adapters::traits::OwnedProcessLaunch`; `mllm_adapters::policy::ParkPolicy` re-exported as `mllm_adapters::ParkPolicy`.

- [ ] **Step 1: Move `Presence`**

In `crates/mllm-domain/src/completion.rs`, after `ProcessIdentity`:

```rust
/// Whether a recorded process still exists, judged by pid, boot id and start ticks
/// together. A pid alone is not an identity: the kernel reuses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The exact recorded process still exists. Never release.
    Alive,
    /// Proven absent: the boot differs, the pid is unused, or the pid was reused by
    /// a different process. Only this authorises release.
    Gone,
    /// Could not be established. Treated as retained, never as absent.
    Unknown,
}
```

Delete the enum from `crates/mllm-launchers/src/process_absence.rs` and add `pub use mllm_domain::completion::Presence;` at its top so existing paths keep compiling.

- [ ] **Step 2: The tool trait**

In `crates/mllm-adapters/src/traits.rs`, after `Launcher`:

```rust
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
```

- [ ] **Step 3: `ParkPolicy` to a product module**

Create `crates/mllm-adapters/src/policy.rs` with the enum cut from `fake/engine.rs` (keep its derives, including `Default`), the doc comment reworded: "Whether the deep-park controls may be called on this engine. Default permits them; a host opts out (spec §1)." Rename the variants to `Enabled` (default) and `Disabled`, and update every match arm the compiler names. Add `pub mod policy; pub use policy::ParkPolicy;` to `lib.rs`, and in `fake/mod.rs` replace the `ParkPolicy` re-export with `pub use crate::policy::ParkPolicy;` so `mllm_adapters::fake::ParkPolicy` still resolves until Task 13.

Run: `grep -rn "ExperimentalAllowed\|ParkPolicy::Denied" crates --include=*.rs` and rename each: `ExperimentalAllowed` → `Enabled`, `Denied` → `Disabled`. The existing T21 test in `crates/mllm-adapters/tests/vllm_adapter.rs` (`park_denied_by_default_without_engine_call`) becomes `park_refused_when_disabled_without_engine_call` and constructs the adapter with `ParkPolicy::Disabled` explicitly; add its sibling `park_permitted_by_default` asserting the default is `Enabled`. Keep `// T21` on both.

- [ ] **Step 4: Compile and test**

Run: `cargo check --offline --workspace --all-targets`
Run: `cargo test --offline -p mllm-domain -p mllm-adapters -p mllm-launchers --all-targets -- --test-threads=4`
Expected: pass. The T21 tests pass with the flipped default.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "refactor: process presence in the domain, launch tools trait, park policy out of the Fake

Presence moves beside ProcessIdentity so adapters and launchers can both name it
without a dependency cycle. OwnedProcessLaunch is the seam a builder spawns,
observes and terminates through. ParkPolicy was defined in the Fake module but is
the vLLM adapter's gate; it moves to a product module and its default flips to
Enabled, because the owner ruled deep parking on by default with a host opt-out."
```

---

### Task 2: `DurableProcessLaunch`: log to a file, dispose of an unreleased child, observe and terminate a group

**Files:**
- Modify: `crates/mllm-launchers/src/durable.rs` (redirect output, dispose on non-ack)
- Modify: `crates/mllm-launchers/src/group_observation.rs` (empty group is an answer)
- Create: `crates/mllm-launchers/src/owned_launch.rs`
- Modify: `crates/mllm-launchers/src/lib.rs` (`pub mod owned_launch; pub use owned_launch::DurableProcessLaunch;`)
- Test: unit tests in `owned_launch.rs` and `durable.rs`

**Interfaces:**
- Consumes: `OwnedProcessLaunch` (Task 1); `DurableSpawn::spawn_persisted`; `LaunchAssociation`; `process_absence::{presence, verify_gone}`; `group_observation::observe_process_group`.
- Produces: `DurableProcessLaunch::new(association: Arc<dyn LaunchAssociation>) -> Self`, implementing `OwnedProcessLaunch`.

- [ ] **Step 1: Failing tests for the launcher changes**

In `crates/mllm-launchers/src/durable.rs` tests, add:

```rust
struct Accept;
impl LaunchAssociation for Accept {
    fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> { Ok(()) }
}
struct Refuse;
impl LaunchAssociation for Refuse {
    fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
        Err(AssociationError::Uncertain("store refused".into()))
    }
}

/// The engine's output lands in the file the plan names, not in /dev/null; the
/// failure path quotes that file (spec §3).
#[test]
fn child_output_is_written_to_the_engine_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs").join("dep").join("inc.log");
    let mut env = std::collections::BTreeMap::new();
    env.insert("MLLM_ENGINE_LOG".to_string(), log.to_str().unwrap().to_string());
    let command = RenderedCommand {
        argv: vec!["sh".into(), "-c".into(), "echo hello-from-engine; echo oops >&2".into()],
        env,
    };
    let launcher = DurableSpawn::new();
    launcher.spawn_persisted("log-test", &command, &Accept).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("hello-from-engine") && text.contains("oops"), "{text}");
    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// A child whose identity is never recorded is not left blocked on its gate
/// (spec §3): the launcher closes the gate, signals the group and reaps it.
#[test]
fn an_unreleased_child_is_disposed_of_before_the_error_returns() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("must-not-run");
    let command = RenderedCommand {
        argv: vec!["sh".into(), "-c".into(), format!("touch '{}'", marker.display())],
        env: Default::default(),
    };
    let launcher = DurableSpawn::new();
    let outcome = launcher.spawn_persisted("refused", &command, &Refuse).unwrap();
    let pid = match outcome {
        DurableSpawnOutcome::Uncertain { handle, initialization_acknowledged: false, .. } => handle.pid,
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "gated child still present");
    assert!(!marker.exists(), "the engine command must never have run");
}
```

Add `use std::os::unix::fs::PermissionsExt;` to the test module.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --offline -p mllm-launchers --lib durable`
Expected: first fails reading the log (no such file); second fails on `/proc/{pid}` existing.

- [ ] **Step 3: Launcher changes**

In `spawn_inner`:
- Before building the command, if `cmd.env` has `MLLM_ENGINE_LOG`, create its parent directories with mode `0o700` (`std::fs::DirBuilder::new().recursive(true).mode(0o700)`), open the file with `OpenOptions::new().create(true).append(true).mode(0o600)`, and use `Stdio::from(file.try_clone()?)` for both stdout and stderr in place of `Stdio::null()`. Without the variable, keep null.
- In the two non-acknowledged branches (identity `None`, association `Err`), before returning: take the `RetainedChild` back out of `retained`, drop its `write_gate` (the child's `dd` reads EOF and exits 125), then `kill(-pid, SIGKILL)` the group as a backstop, `wait()` the child, and only then return the `Uncertain` outcome. Add a `disposed: true` note to the reason string.

Keep `detach_reaper` for the acknowledged path only.

In `group_observation.rs`, add:

```rust
/// Like `observe_process_group`, but a group whose leader is gone is reported as
/// empty rather than as an error, because cleanup needs "nothing is left" as an
/// answer. Any other failure still fails closed.
pub fn observe_process_group_or_empty(
    expected_api: &ProcessIdentity,
) -> Result<Vec<GroupProcessFact>, GroupObservationError> {
    match super::process_absence::presence(expected_api) {
        Presence::Gone => Ok(Vec::new()),
        Presence::Unknown => Err(GroupObservationError::Visibility),
        Presence::Alive => observe_process_group(expected_api).map(|o| o.members().to_vec()),
    }
}
```

Note: when the leader is gone but a worker survives in the same process group, this returns empty and hides the worker. Add a second pass: scan `/proc/*/stat` for processes whose `pgrp` equals `expected_api.pid` (the group id is the leader's pid) and whose boot id matches; return those as members even when the leader is gone. Implement `scan_group_by_pgid(pgid: u32, boot: &str) -> Result<Vec<GroupProcessFact>, GroupObservationError>` using the same bounded `/proc` reads the file already has, and use it in the `Gone` branch instead of `Vec::new()`.

- [ ] **Step 4: `DurableProcessLaunch`**

```rust
//! The one implementation of the builder's process tools (spec §3, §5).
use std::sync::Arc;
use std::time::{Duration, Instant};

use mllm_adapters::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeError};
use mllm_domain::completion::{Presence, ProcessIdentity};

use crate::durable::{DurableSpawn, DurableSpawnOutcome, LaunchAssociation};
use crate::group_observation::observe_process_group_or_empty;
use crate::process_absence::{presence, verify_gone, GoneProof};

pub struct DurableProcessLaunch {
    spawn: DurableSpawn,
    association: Arc<dyn LaunchAssociation>,
}

impl DurableProcessLaunch {
    pub fn new(association: Arc<dyn LaunchAssociation>) -> Self {
        Self { spawn: DurableSpawn::new(), association }
    }
}

fn uncertain(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::Uncertain(reason.into())
}

impl OwnedProcessLaunch for DurableProcessLaunch {
    fn spawn_durable(&self, incarnation: &str, cmd: &RenderedCommand) -> Result<ProcessIdentity, RuntimeError> {
        match self.spawn.spawn_persisted(incarnation, cmd, self.association.as_ref()) {
            Ok(DurableSpawnOutcome::Uncertain { api_identity: Some(identity), initialization_acknowledged: true, .. }) => Ok(identity),
            Ok(DurableSpawnOutcome::Uncertain { reason, .. }) => Err(uncertain(format!("launch not released: {reason}"))),
            Err(error) => Err(uncertain(format!("spawn failed: {error}"))),
        }
    }

    fn present(&self, identity: &ProcessIdentity) -> Presence {
        presence(identity)
    }

    fn observe_group(&self, api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        let facts = observe_process_group_or_empty(api).map_err(|e| uncertain(e.to_string()))?;
        let mut members: Vec<ProcessIdentity> = Vec::new();
        let mut workers: Vec<_> = facts.iter().filter(|f| f.pid != api.pid).collect();
        workers.sort_by_key(|f| (f.start_ticks, f.pid));
        if facts.iter().any(|f| f.pid == api.pid && f.start_ticks == api.start_ticks) {
            members.push(api.clone());
        }
        for (index, fact) in workers.into_iter().enumerate() {
            members.push(ProcessIdentity {
                role: format!("worker-{index}"),
                pid: fact.pid,
                boot_id: fact.boot_id.clone(),
                start_ticks: fact.start_ticks,
            });
        }
        Ok(members)
    }

    fn terminate_owned(&self, identities: &[ProcessIdentity], grace: Duration) -> Result<(), RuntimeError> {
        let api = identities.iter().find(|i| i.role == "api");
        for identity in identities {
            if presence(identity) == Presence::Unknown {
                return Err(uncertain(format!("presence of pid {} could not be established", identity.pid)));
            }
        }
        let Some(api) = api else {
            return match verify_gone(identities) {
                GoneProof::AllGone => Ok(()),
                _ => Err(uncertain("no API identity recorded and the set is not proven gone")),
            };
        };
        // SPEC §13.2: signal only a group whose leader is provably ours.
        if presence(api) == Presence::Alive {
            signal_group(api.pid, nix::sys::signal::Signal::SIGTERM)?;
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if verify_gone(identities) == GoneProof::AllGone && self.observe_group(api)?.is_empty() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            signal_group(api.pid, nix::sys::signal::Signal::SIGKILL)?;
            let hard = Instant::now() + Duration::from_secs(5);
            while Instant::now() < hard {
                if verify_gone(identities) == GoneProof::AllGone && self.observe_group(api)?.is_empty() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        match (verify_gone(identities), self.observe_group(api)?.is_empty()) {
            (GoneProof::AllGone, true) => Ok(()),
            (GoneProof::AllGone, false) => Err(uncertain("recorded processes gone but the group still has members")),
            (GoneProof::SomeAlive, _) => Err(uncertain("a recorded process is still alive after SIGKILL")),
            (GoneProof::Indeterminate, _) => Err(uncertain("process absence could not be established")),
        }
    }
}

fn signal_group(leader_pid: u32, signal: nix::sys::signal::Signal) -> Result<(), RuntimeError> {
    let pgid = nix::unistd::Pid::from_raw(-(leader_pid as i32));
    match nix::sys::signal::kill(pgid, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(uncertain(format!("signal failed: {error}"))),
    }
}
```

`presence()` checks boot id and start ticks against `/proc/<pid>/stat`; that is the PID-reuse refusal. Reuse `ExecLauncher`'s escalation pattern; if a helper there can be shared without changing its behavior, share it.

- [ ] **Step 5: Tests for the tool**

In `owned_launch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    struct Accept;
    impl LaunchAssociation for Accept {
        fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), crate::durable::AssociationError> { Ok(()) }
    }
    fn sleeper(seconds: u32) -> RenderedCommand {
        RenderedCommand {
            // A leader that spawns one child in the same group, like an engine with a worker.
            argv: vec!["sh".into(), "-c".into(), format!("sleep {seconds} & sleep {seconds}")],
            env: Default::default(),
        }
    }

    /// The group is enumerated with the API process first and workers in start
    /// order, and terminating it leaves nothing behind. T20
    #[test]
    fn a_group_is_observed_and_terminated_completely() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("grp", &sleeper(60)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let members = tool.observe_group(&api).unwrap();
        assert_eq!(members[0].role, "api");
        assert!(members.len() >= 2, "{members:?}");
        assert_eq!(members[1].role, "worker-0");
        tool.terminate_owned(&members, Duration::from_secs(2)).unwrap();
        assert_eq!(tool.present(&api), Presence::Gone);
        assert!(tool.observe_group(&api).unwrap().is_empty());
    }

    /// A pid that now names a different process is never signalled.
    #[test]
    fn a_reused_pid_is_refused() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("reuse", &sleeper(60)).unwrap();
        let forged = ProcessIdentity { start_ticks: api.start_ticks + 1, ..api.clone() };
        assert_eq!(tool.present(&forged), Presence::Gone);
        // Cleanup of the real one so the test leaves nothing behind.
        tool.terminate_owned(&[api], Duration::from_secs(1)).unwrap();
    }
}
```

- [ ] **Step 6: Run and commit**

Run: `cargo test --offline -p mllm-launchers --all-targets -- --test-threads=1`
Expected: pass; the launcher tests start real processes and must not run in parallel with each other.

```bash
git add crates/mllm-launchers
git commit -m "feat(launchers): durable process tools for a native builder

DurableProcessLaunch spawns behind the identity gate with the engine's output in
its log file, disposes of a child whose identity was never recorded instead of
leaving a blocked shell, enumerates the launched group with the API process first
and workers in start order, and terminates a group only when its leader is
provably the recorded process, proving every member gone afterwards."
```

---

### Task 3: Store accepts the early identity and any number of workers

**Files:**
- Modify: `crates/mllm-store/src/ordinary_lifecycle.rs` (`record_launch` precondition), `crates/mllm-store/src/lifecycle/completion.rs` (`canonical_members`, `members`)
- Test: `crates/mllm-store/src/ordinary_lifecycle/tests.rs`

**Interfaces:**
- Produces: `record_launch` accepts a binding whose `identities_json` is `[]` or exactly the API identity the association wrote; `canonical_members` accepts `api` plus `worker-0..worker-N`.

- [ ] **Step 1: Failing tests**

In `crates/mllm-store/src/ordinary_lifecycle/tests.rs`, beside the existing owned-launch tests (read one to copy its setup):

```rust
/// Spec §3: the durable launcher records the API identity before the engine runs.
/// Recording the completed launch must accept that, and only that, prior content.
#[test]
fn record_launch_accepts_the_api_identity_the_association_wrote() {
    // Setup: accept a start, arm it, obtain (session, step_id, binding_id, fence)
    // exactly as `owned_launch_records_association` does in this file.
    // Then, before record_owned_launch:
    store.record_api_identity(&session, &fence, &binding_id, &api).unwrap();
    let receipt = OwnedLaunchReceipt { binding_id, incarnation, identities: vec![api.clone(), worker0.clone()], observed_at_ms: now, receipt: "vllm ready".into() };
    store.record_owned_launch(&session, &step_id, &receipt, now).unwrap();
}

/// A binding holding a different identity than the receipt's API process is refused.
#[test]
fn record_launch_refuses_a_mismatched_prior_identity() {
    // same setup; record_api_identity with pid 999, then a receipt whose api has pid 7
    assert!(matches!(store.record_owned_launch(&session, &step_id, &receipt, now), Err(LifecycleError::Conflict)));
}

/// Spec §4: a tensor-parallel launch has several workers. The store accepts api plus
/// worker-0..worker-N and still refuses a set without a worker.
#[test]
fn canonical_members_accepts_many_workers_and_refuses_none() {
    let api = identity("api", 10); let w0 = identity("worker-0", 11); let w1 = identity("worker-1", 12);
    assert!(canonical_members(&[api.clone(), w0.clone(), w1.clone()]).is_ok());
    assert!(canonical_members(&[api.clone()]).is_err());
    assert!(canonical_members(&[api.clone(), identity("worker-1", 12)]).is_err(), "workers are contiguous from 0");
}
```

- [ ] **Step 2: Run, watch fail**

Run: `cargo test --offline -p mllm-store --lib ordinary_lifecycle::tests::record_launch -- --nocapture` and `canonical_members_accepts`
Expected: first fails `Conflict` at the `identities_json='[]'` check; third fails on length.

- [ ] **Step 3: Implement**

`canonical_members` in `lifecycle/completion.rs`: require `len >= 2`, sort, `ids[0].role == "api"`, and for `i in 1..len` require `ids[i].role == format!("worker-{}", i - 1)`, all same boot id, distinct pids. `members` (its sibling used for stored associations) gets the same rule. Update the doc comment: "api plus one or more workers named contiguously from worker-0 (spec §4)".

`record_launch` in `ordinary_lifecycle.rs`: replace the `identities_json='[]'` query with reading the column, decoding `Vec<IdentityDto>`, and accepting when it is empty or when it equals `identity_dtos(&[receipt api identity])` where the receipt's API identity is `canonical_members(&r.identities)?[0]`. Anything else is `Conflict`. Cite `// Spec §3: the association wrote the API identity before the engine ran.`

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-store --all-targets -- --test-threads=4`
Expected: pass.

```bash
git add crates/mllm-store
git commit -m "feat(store): a launch may record its API identity early and several workers

The durable launcher writes the API process identity before the engine runs.
Recording the completed launch now accepts exactly that prior content, and the
member rule accepts api plus workers numbered contiguously from zero, which a
tensor-parallel launch needs and vLLM's EngineCore already requires."
```

---

### Task 4: Encrypted engine keys at rest

**Files:**
- Modify: `crates/mllm-store/Cargo.toml` (add `chacha20poly1305 = "0.10"`, `rand_core` via its `getrandom` feature), `Cargo.lock`
- Modify: `crates/mllm-store/src/schema.rs`, `crates/mllm-store/src/migrations.rs` (v14)
- Create: `crates/mllm-store/src/secrets.rs`; Modify: `crates/mllm-store/src/lib.rs`
- Modify: `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs` (delete the key row on release)

**Interfaces:**
- Produces: `SecretsKey::load_or_create(path: &Path) -> Result<SecretsKey, StoreError>`; `Store::set_secrets_key(&mut self, key: SecretsKey)`; `Store::store_engine_key(&self, binding_id, incarnation, key: &[u8; 32])`; `Store::engine_key(&self, binding_id, incarnation) -> Result<Option<[u8; 32]>, StoreError>`; `Store::delete_engine_key(&self, binding_id)`; `SCHEMA_V14`.

- [ ] **Step 1: Dependency**

Run (network needed once): `cargo add -p mllm-store chacha20poly1305@0.10 --features std` and `cargo add -p mllm-store rand_core@0.6 --features getrandom`. Commit `Cargo.lock` with the task.

- [ ] **Step 2: Failing tests**

`crates/mllm-store/src/secrets.rs` tests:

```rust
/// A key round-trips only under the same identity key and the same row identity.
#[test]
fn engine_key_round_trips_and_is_bound_to_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretsKey::load_or_create(&dir.path().join("secrets.key")).unwrap();
    let mut store = crate::Store::open_in_memory().unwrap();
    store.set_secrets_key(secrets);
    seed_binding(&store, "b1", "inc1"); // helper: insert a runtime_bindings row as other tests do
    let key = [7u8; 32];
    store.store_engine_key("b1", "inc1", &key).unwrap();
    assert_eq!(store.engine_key("b1", "inc1").unwrap(), Some(key));
    // Same ciphertext under another binding does not authenticate.
    store.conn.execute("UPDATE engine_secrets SET binding_id='b2' WHERE binding_id='b1'", []).unwrap();
    assert!(store.engine_key("b2", "inc1").is_err());
}

#[test]
fn the_identity_key_file_is_owner_only_and_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.key");
    let a = SecretsKey::load_or_create(&path).unwrap();
    let b = SecretsKey::load_or_create(&path).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn v14_adds_engine_secrets_and_preserves_rows() { /* same shape as v13_drops_qualification_tables_and_preserves_rows, asserting the table exists */ }
```

- [ ] **Step 3: Schema and module**

`schema.rs`:

```rust
/// Spec §3: the per-launch engine key, encrypted at rest with XChaCha20-Poly1305 under
/// the identity key file, with binding id and incarnation as associated data so a row
/// copied between bindings does not authenticate. Deleted when the binding releases.
pub const SCHEMA_V14: &str = r#"
CREATE TABLE engine_secrets(
  binding_id TEXT PRIMARY KEY REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL,
  nonce BLOB NOT NULL CHECK(length(nonce)=24),
  ciphertext BLOB NOT NULL
);
"#;
```

Add to both lists in `migrations.rs`.

`secrets.rs`:

```rust
//! The engine key at rest (spec §3). The database alone cannot recover a key: the
//! identity key lives in `<state_dir>/identity/secrets.key`, owner-only.
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use chacha20poly1305::{aead::{Aead, KeyInit, Payload}, XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, OptionalExtension};
use crate::StoreError;

#[derive(Clone)]
pub struct SecretsKey([u8; 32]);
impl SecretsKey {
    pub fn load_or_create(path: &Path) -> Result<Self, StoreError> {
        if let Ok(bytes) = std::fs::read(path) {
            let key: [u8; 32] = bytes.try_into().map_err(|_| StoreError::Conflict)?;
            return Ok(Self(key));
        }
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?; }
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        std::io::Write::write_all(&mut file, &key)?;
        Ok(Self(key))
    }
    pub fn generate_ephemeral() -> Self { let mut k = [0u8; 32]; OsRng.fill_bytes(&mut k); Self(k) }
    pub fn fingerprint(&self) -> String { hex::encode(sha2::Sha256::digest(self.0)) }
}
impl std::fmt::Debug for SecretsKey { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("SecretsKey(..)") } }

pub fn new_engine_key() -> [u8; 32] { let mut k = [0u8; 32]; OsRng.fill_bytes(&mut k); k }

impl crate::Store {
    pub fn set_secrets_key(&mut self, key: SecretsKey) { self.secrets = Some(key); }
    fn cipher(&self) -> Result<XChaCha20Poly1305, StoreError> {
        let key = self.secrets.as_ref().ok_or(StoreError::Conflict)?;
        Ok(XChaCha20Poly1305::new((&key.0).into()))
    }
    pub fn store_engine_key(&self, binding_id: &str, incarnation: &str, key: &[u8; 32]) -> Result<(), StoreError> {
        let mut nonce = [0u8; 24]; OsRng.fill_bytes(&mut nonce);
        let aad = format!("{binding_id}\0{incarnation}");
        let ciphertext = self.cipher()?.encrypt(XNonce::from_slice(&nonce), Payload { msg: key, aad: aad.as_bytes() }).map_err(|_| StoreError::Conflict)?;
        self.conn.execute("INSERT OR REPLACE INTO engine_secrets(binding_id,incarnation,nonce,ciphertext) VALUES(?1,?2,?3,?4)", params![binding_id, incarnation, nonce.to_vec(), ciphertext])?;
        Ok(())
    }
    pub fn engine_key(&self, binding_id: &str, incarnation: &str) -> Result<Option<[u8; 32]>, StoreError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self.conn.query_row("SELECT nonce,ciphertext FROM engine_secrets WHERE binding_id=?1 AND incarnation=?2", params![binding_id, incarnation], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((nonce, ciphertext)) = row else { return Ok(None) };
        let aad = format!("{binding_id}\0{incarnation}");
        let plain = self.cipher()?.decrypt(XNonce::from_slice(&nonce), Payload { msg: &ciphertext, aad: aad.as_bytes() }).map_err(|_| StoreError::Conflict)?;
        plain.try_into().map(Some).map_err(|_| StoreError::Conflict)
    }
    pub fn delete_engine_key(&self, binding_id: &str) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM engine_secrets WHERE binding_id=?1", [binding_id])?; Ok(())
    }
}
```

Add `secrets: Option<SecretsKey>` to `Store`, `None` in `open`/`open_in_memory`. Add `hex` to the store's dependencies if not present (it is a workspace dependency). Authentication failure surfaces as `StoreError::Conflict`; the caller (Task 9) maps it to `Uncertain`.

In `ordinary_lifecycle/cleanup.rs::complete`, next to `DELETE FROM endpoint_leases`, add `tx.execute("DELETE FROM engine_secrets WHERE binding_id=?1", [&p.receipt.binding_id])?;` (inside the transaction; `tx` is available). Task 5 does the same in `release_failed_launch`.

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-store --all-targets -- --test-threads=4` (offline works once the lockfile has the crates)
Expected: pass.

```bash
git add Cargo.lock crates/mllm-store
git commit -m "feat(store): engine keys encrypted at rest

Schema v14 adds engine_secrets. A per-launch key is sealed with
XChaCha20-Poly1305 under a 32-byte identity key file that never enters the
database, with binding id and incarnation as associated data so a copied row does
not authenticate. Ordinary cleanup deletes the row with the binding."
```

---

### Task 5: `release_failed_launch`

**Files:**
- Create: `crates/mllm-store/src/ordinary_lifecycle/failed_launch.rs`; Modify: `crates/mllm-store/src/ordinary_lifecycle.rs` (`mod failed_launch;`)
- Test: in the new file

**Interfaces:**
- Consumes: `CleanupEvidence` (domain), `canonical_members`, `members`, `advance_completion_epoch`, the cleanup `complete` transaction as the template.
- Produces: `Store::release_failed_launch(&self, session: &CoordinatorSession, step_id: &str, evidence: &CleanupEvidence, now: i64, ttl: i64) -> Result<(), LifecycleError>`; `Store::observation_ttl_for_step(&self, step_id: &str) -> Result<i64, LifecycleError>` (the host's `observation_ttl_ms` from the step's effective revision, so the caller passes the same ttl the guard checks); `Store::runtime_binding_identities(&self, binding_id: &str) -> Result<Vec<ProcessIdentity>, LifecycleError>`.

- [ ] **Step 1: Failing tests**

```rust
/// Spec §6: a launch that failed after arm is released with gone evidence. The step
/// is cancelled, the run and operation fail, the binding, lease, grant and claim are
/// released, the key row is deleted, and the deployment keeps desired_state ready.
#[test]
fn a_failed_launch_is_released_with_evidence() {
    // Setup: accept a start, arm it, record_api_identity(api), store_engine_key.
    let evidence = CleanupEvidence { binding_id, incarnation, identities: vec![api.clone()], observed_at_ms: now, receipt: "every recorded process observed gone".into() };
    store.release_failed_launch(&session, &step_id, &evidence, now, ttl).unwrap();
    assert_eq!(step_state(&store, &step_id), "cancelled");
    assert_eq!(run_state(&store, &operation_id), "failed");
    assert_eq!(binding_state(&store, &binding_id), "released");
    assert_eq!(store.scalar("SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1", &binding_id), 0);
    assert_eq!(store.scalar("SELECT COUNT(*) FROM resource_owners WHERE owner_id=?1", &deployment_id), 0);
    assert_eq!(store.scalar("SELECT COUNT(*) FROM engine_secrets WHERE binding_id=?1", &binding_id), 0);
    assert_eq!(desired_state(&store, &deployment_id), "ready");
    // Replay with identical evidence is accepted; different evidence is refused.
    store.release_failed_launch(&session, &step_id, &evidence, now + 1, ttl).unwrap();
    let other = CleanupEvidence { receipt: "different".into(), ..evidence.clone() };
    assert!(matches!(store.release_failed_launch(&session, &step_id, &other, now + 1, ttl), Err(LifecycleError::Conflict)));
}

/// Spec §6 step 1: no recorded identity is itself the evidence; the identity set is empty.
#[test]
fn a_never_released_launch_is_released_with_an_empty_identity_set() {
    // Setup: accept + arm, do NOT record_api_identity.
    let evidence = CleanupEvidence { identities: vec![], receipt: "gate never opened; child disposed".into(), .. };
    store.release_failed_launch(&session, &step_id, &evidence, now, ttl).unwrap();
}

/// Spec §6: the evidence's identity set must equal what was recorded.
#[test]
fn mismatched_identities_are_refused() { /* record api pid 7, evidence with pid 8 → Conflict */ }

/// Spec §6: a deadline-triggered failure proves gone after the deadline; that is accepted.
#[test]
fn evidence_after_the_step_deadline_is_accepted_when_fresh() {
    // now = deadline_ms + 5_000, observed_at_ms = now - 100, ttl = 2_000 → Ok
}
```

- [ ] **Step 2: Run, watch fail**

Run: `cargo test --offline -p mllm-store --lib failed_launch`
Expected: method not found.

- [ ] **Step 3: Implement**

Model on `cleanup::complete`. Differences, each with a comment:

```rust
/// Spec §6: release a launch that failed after arm, with proof nothing it started
/// remains. Same guards as ordinary cleanup, except the step-deadline bound: a
/// deadline-triggered failure proves the process gone after that deadline by
/// construction, so `fresh`'s `now > deadline` rejection is not applied here.
pub fn release_failed_launch(&self, s: &CoordinatorSession, id: &str, evidence: &CleanupEvidence, now: i64, ttl: i64) -> Result<(), LifecycleError>
```

Body, in one `Immediate` transaction:
1. `check_session`; `load(tx, id)` the initialize step; `current_admitted(tx, s, &plan, false, false)` (admission may already be closed).
2. If `lifecycle_evidence` already has this step: identical encoded evidence → `Ok(())`, else `Conflict`.
3. State must be `armed` or `uncertain`.
4. Read `runtime_bindings.identities_json`; decode; require `canonical_members_or_empty(&evidence.identities) == recorded` where an empty recorded set matches only an empty evidence set, and a non-empty one uses `members`. Evidence `binding_id`/`incarnation` must match the plan.
5. `ttl == e.host.observation_ttl_ms`, else `Invalid`. Freshness: `observed >= issued_at`, `now >= observed`, `now <= observed + ttl`. No deadline check.
6. Writes: `UPDATE lifecycle_steps SET state='cancelled' WHERE id=?1 AND state IN ('armed','uncertain')`; `UPDATE lifecycle_runs SET state='failed'`; `UPDATE operations SET state='failed', error_code='launch_failed'`; `DELETE FROM resource_owners WHERE owner_id=?deployment`; `DELETE FROM endpoint_leases WHERE binding_id=?`; `UPDATE runtime_bindings SET state='released' WHERE id=?`; `DELETE FROM engine_secrets WHERE binding_id=?`; `DELETE FROM lifecycle_claims WHERE operation_id=?`; `DELETE FROM request_leases` for the fence. Do **not** touch `deployments.desired_state`.
7. `let epoch = resource_ledger::advance_completion_epoch(tx)?;` insert `lifecycle_evidence(step_id, evidence_json, epoch)`. The epoch advances because a grant was released; that is what the epoch counts.
8. Emit event `LifecycleTransition::LaunchFailed` (add the variant to `events.rs` with kind string `initialize_failed_released`).

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-store --all-targets -- --test-threads=4`

```bash
git add crates/mllm-store
git commit -m "feat(store): release a failed launch with gone evidence

A launch that fails after arm is released in one transaction once its recorded
processes are proven gone: the step is cancelled, the run and operation fail, the
binding, port lease, grant, claim and engine key are released, and the evidence is
written at a new completion epoch. The guards match ordinary cleanup except the
step-deadline bound, which a timeout failure cannot satisfy by construction."
```

---

### Task 6: Configuration: park switch, trust-remote-code, model store, model source, five settings, budget bound

**Files:**
- Modify: `crates/mllm-config/src/schema.rs`, `crates/mllm-config/src/effective.rs`, `crates/mllm-config/src/effective/core.rs`, `crates/mllm-config/src/engine_policy.rs`
- Test: `crates/mllm-config/tests/effective.rs`

**Interfaces:**
- Produces on `EffectiveDeployment`: `profile.security.deep_park: DeepPark { Enabled, Disabled }` (replaces `experimental_controls: bool`), `profile.security.trust_remote_code: bool`, `host.model_store: PathBuf`, `model.source: ModelSource { Local { path }, HuggingFace { repo, revision: Option<String>, locked_commit: Option<String> }, Http { url, sha256 } }`, `model.resolved_path: String`.

- [ ] **Step 1: Failing tests** (one per rule, in `tests/effective.rs`, using the file's existing builders)

```rust
// Spec §3: disabled deep_park refuses a parking residency and turns sleep mode off.
#[test] fn deep_park_disabled_with_parking_residency_is_refused() { /* residency: deep, security.deep_park: disabled → Err naming both */ }
#[test] fn deep_park_defaults_to_enabled() { /* omit the key → profile.security.deep_park == Enabled */ }
// Spec §3: --trust-remote-code needs the host switch.
#[test] fn trust_remote_code_arg_needs_the_host_switch() { /* args: ["--trust-remote-code"], switch absent → Err; switch true → Ok */ }
// Spec §7: model store required; local source resolves relative to it; any absolute path allowed.
#[test] fn model_store_is_required_and_local_paths_resolve_against_it() { /* host without model_store → Err; source {type: local, path: "qwen3-4b"} → resolved_path == "<store>/qwen3-4b"; absolute "/anywhere/x" → "/anywhere/x" */ }
#[test] fn huggingface_and_http_sources_validate_shape_but_are_not_materializable() { /* huggingface {repo, revision omitted} → Ok shape, resolved_path Err(NotMaterializable); http requires https */ }
// Spec §3: requested KV may not exceed the Ready allocation.
#[test] fn requested_kv_above_ready_allocation_is_refused() {}
// Spec §7: legacy `model: { path }` still parses as a local source.
#[test] fn legacy_model_path_is_a_local_source() {}
```

- [ ] **Step 2: Run, watch fail**

- [ ] **Step 3: Implement**

`schema.rs`: `SECURITY` gains `("deep_park", SCALAR)` and `("trust_remote_code", SCALAR)`, drops `experimental_controls`. Host schema gains `("model_store", FieldSpec::Struct(&[("path", SCALAR)]))`, required. `MODEL` gains `("source", FieldSpec::Struct(&[("type", SCALAR), ("path", SCALAR), ("repo", SCALAR), ("revision", SCALAR), ("locked_commit", SCALAR), ("url", SCALAR), ("sha256", SCALAR)]))`; `path` at the model level stays accepted as the legacy spelling of a local source.

`effective.rs`: `Security { deep_park: DeepPark, trust_remote_code: bool, credential_ref, admin_credential_ref }` with `DeepPark::default() == Enabled`. `HostPolicy.model_store: PathBuf`. `ModelIdentity` gains `source: ModelSource` and `resolved_path: String`. Validation: `deep_park == Disabled && residency.parks()` → `invalid("runtime_profiles.security.deep_park", "a parking deployment cannot run on a profile that disables deep park; set deep_park: enabled or residency: restart_only")`. `normalize_launch` no longer requires `enable_sleep_mode` for parking; instead Task 7's plan derives sleep flags from `enable_sleep_mode && deep_park == Enabled`. `--trust-remote-code` in `profile.args` without `trust_remote_code: true` → `invalid(...)`. `requested_budget.kv_cache_bytes > resources.ready.allocations[domain].bytes` → `invalid("runtime_profiles.launch_settings.requested_budget", "requested KV exceeds the Ready allocation admission accounts for")`. `http` source: `url` must start with `https://`, `sha256` is 64 hex.

`engine_policy.rs`: leave `--trust-remote-code` on the approved list; the switch check lives in `effective.rs`.

Standalone (`mllm-cli`) is updated in Task 14; until then its host policy fails validation, so Task 14 is not optional.

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-config --all-targets -- --test-threads=4`; `cargo check --offline --workspace --all-targets` will fail in `mllm-cli` and `mllm-controller` until Tasks 9 and 14; that is expected and noted in the commit.

```bash
git add crates/mllm-config
git commit -m "feat(config): park opt-out, trust-remote-code switch, model store and model source

security.deep_park replaces experimental_controls and defaults to enabled; a
parking deployment on a disabled profile is refused. --trust-remote-code needs the
host's explicit switch. A host declares its model store; a deployment names a model
source (local, huggingface, http) and only local resolves before slice S1b. A
requested KV budget above the Ready allocation is refused so the rendered grant
never exceeds what admission accounted for."
```

---

### Task 7a: The control-route guard in `runtime/`

vLLM 0.29 authenticates only `/v1`, `/v2`, `/inference` and `/cohere` (verified on the
box, `vllm/entrypoints/serve/middleware/authenticate.py`, `GUARDED_PREFIX`). The
development routes mllm parks through are open. Spec §3 makes the guard a requirement.

**Files:**
- Create: `runtime/mllm_vllm_guard.py`, `runtime/tests/test_mllm_vllm_guard.py`

**Interfaces:**
- Produces: importable `mllm_vllm_guard.RequireEngineKey`, an ASGI middleware class taking `(app)`; reads the key from `VLLM_API_KEY` at construction; 401 JSON `{"error":"Unauthorized"}` on any HTTP or websocket path except `/health` without a matching bearer; OPTIONS passes.

- [ ] **Step 1: Failing test** (`runtime/tests/test_mllm_vllm_guard.py`, using Starlette's `TestClient` as the other runtime tests do; confirm `starlette` is importable in the test venv, else use a minimal ASGI scope harness)

```python
def test_dev_routes_require_the_engine_key(monkeypatch):
    monkeypatch.setenv("VLLM_API_KEY", "k3y")
    app = RequireEngineKey(echo_app)  # echo_app answers 200 on any path
    client = TestClient(app)
    assert client.post("/sleep").status_code == 401
    assert client.post("/collective_rpc").status_code == 401
    assert client.post("/sleep", headers={"Authorization": "Bearer k3y"}).status_code == 200
    assert client.get("/health").status_code == 200
    assert client.get("/v1/models").status_code == 401  # belt and braces with vLLM's own guard
def test_missing_key_env_refuses_everything(monkeypatch):
    monkeypatch.delenv("VLLM_API_KEY", raising=False)
    with pytest.raises(RuntimeError): RequireEngineKey(echo_app)
```

- [ ] **Step 2: Implement** (mirror vLLM's own middleware: hash the token, `secrets.compare_digest`, pure ASGI, no request body read)

- [ ] **Step 3: Run and commit**

Run: `cd runtime && python -m pytest tests/test_mllm_vllm_guard.py -q`

```bash
git add runtime/mllm_vllm_guard.py runtime/tests/test_mllm_vllm_guard.py
git commit -m "feat(runtime): guard vLLM's development routes with the engine key

vLLM authenticates only its inference prefixes; the sleep, wake and collective_rpc
routes mllm parks through are open to any local caller. This middleware, loaded by
mllm through --middleware, requires the engine key on every path except /health."
```

---

### Task 7: vLLM command rendering: owned flags, five settings, no key on argv

**Files:**
- Modify: `crates/mllm-adapters/src/vllm/args.rs` (`PlanInputVllm`, `render_command`), `crates/mllm-adapters/tests/vllm_args.rs` (or the existing args test file)

**Interfaces:**
- Produces: `PlanInputVllm { engine_bin, model_path, port, served_model_name: String, tensor_parallel_size: u32, pipeline_parallel_size: u32, kv_cache_dtype: String, block_size_tokens: u32, cpu_offload_bytes: i64, granted, engine_args, sleep_flags, runtime_dir: Option<String>, api_key: Option<String>, engine_path_extra, engine_log }` and `render_command` emitting `--host 127.0.0.1`, `--served-model-name`, `--tensor-parallel-size`, `--pipeline-parallel-size`, `--kv-cache-dtype`, `--block-size`, `--cpu-offload-gb` (only when `cpu_offload_bytes > 0`, bytes to whole GiB), and, whenever `sleep_flags` contains `--enable-sleep-mode`, `--middleware mllm_vllm_guard.RequireEngineKey` with `PYTHONPATH=<runtime_dir>` in the environment (spec §3: the guard is owned by mllm; `--middleware` joins the reserved list so a profile cannot pass its own).

- [ ] **Step 1: Failing tests**

```rust
/// Spec §3: mllm owns the listener address and the served name; profiles cannot set them.
#[test]
fn render_emits_host_and_served_name() {
    let cmd = render_command(&plan()).unwrap();
    assert_flag(&cmd, "--host", "127.0.0.1");
    assert_flag(&cmd, "--served-model-name", "gate-m");
}
/// Spec §3: every validated launch setting reaches the engine.
#[test]
fn render_emits_the_five_launch_settings() {
    let mut p = plan(); p.tensor_parallel_size = 2; p.pipeline_parallel_size = 1; p.kv_cache_dtype = "fp8".into(); p.block_size_tokens = 32; p.cpu_offload_bytes = 4 * 1024 * 1024 * 1024;
    let cmd = render_command(&p).unwrap();
    assert_flag(&cmd, "--tensor-parallel-size", "2"); assert_flag(&cmd, "--pipeline-parallel-size", "1");
    assert_flag(&cmd, "--kv-cache-dtype", "fp8"); assert_flag(&cmd, "--block-size", "32"); assert_flag(&cmd, "--cpu-offload-gb", "4");
    let mut none = plan(); none.cpu_offload_bytes = 0;
    assert!(!render_command(&none).unwrap().argv.contains(&"--cpu-offload-gb".to_string()));
}
/// Spec §3: development mode always comes with mllm's guard, and only then.
#[test]
fn dev_mode_renders_the_guard_middleware() {
    let mut p = plan(); p.sleep_flags = vec!["--enable-sleep-mode".into()]; p.runtime_dir = Some("/opt/mllm/runtime".into());
    let cmd = render_command(&p).unwrap();
    assert_flag(&cmd, "--middleware", "mllm_vllm_guard.RequireEngineKey");
    assert_eq!(cmd.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str), Some("1"));
    assert!(cmd.env.get("PYTHONPATH").unwrap().starts_with("/opt/mllm/runtime"));
    let mut off = plan(); off.sleep_flags.clear();
    assert!(!render_command(&off).unwrap().argv.contains(&"--middleware".to_string()));
    let mut no_dir = plan(); no_dir.sleep_flags = vec!["--enable-sleep-mode".into()]; no_dir.runtime_dir = None;
    assert!(render_command(&no_dir).is_err(), "dev mode without a runtime dir cannot be rendered");
}
/// Spec §3: the key never reaches argv even if a caller sets it on the plan.
#[test]
fn render_never_emits_api_key() {
    let mut p = plan(); p.api_key = Some("secret".into());
    assert!(!render_command(&p).unwrap().argv.iter().any(|a| a == "--api-key" || a == "secret"));
}
```

- [ ] **Step 2: Run, watch fail. Step 3: Implement.**

Add the fields; push `--host 127.0.0.1` right after `serve <model>`, then `--served-model-name`, then the five settings, then budget flags as today. Delete the `--api-key` emission entirely and the `api_key` field's use; keep the field so `fingerprint_of` redaction keeps compiling, documented as "never rendered; delivered through the environment by the builder". `swap_gib`-style conversion for offload: `bytes / (1024*1024*1024)`, and refuse (`InvalidBudget`) a positive value below 1 GiB, since vLLM takes whole GiB.

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-adapters --all-targets -- --test-threads=4`

```bash
git add crates/mllm-adapters
git commit -m "feat(adapters): vLLM command carries the owned flags and every launch setting

mllm renders --host 127.0.0.1 and --served-model-name itself, since profiles may
not pass them, and renders tensor and pipeline parallelism, KV cache dtype, block
size and CPU offload from the validated launch settings, which were accepted and
then silently dropped before. The engine key is never rendered on argv."
```

---

### Task 8: `VllmAdapter::execute_persisted(Initialize)`

**Files:**
- Create: `crates/mllm-adapters/src/vllm/initialize.rs`; Modify: `crates/mllm-adapters/src/vllm/adapter.rs` (fields `tools: Option<Arc<dyn OwnedProcessLaunch>>`, `launched: Mutex<Option<(String, String, ProcessIdentity)>>`, `engine_key: Option<String>`; `with_tools`, `with_engine_key`; `execute_persisted` delegating), `crates/mllm-adapters/src/resolve.rs` (`resolve(declared, spec, tools)`; `AdapterSpec::Vllm` gains `launch: Option<PlanInputVllm>`, `engine_key: Option<String>`)
- Test: `crates/mllm-adapters/tests/vllm_initialize.rs` with an `axum` stub engine and a scripted fake tool

**Interfaces:**
- Consumes: `OwnedProcessLaunch` (Task 1), `render_command` (Task 7), `check_readiness`, `forward_chat`.
- Produces: `VllmAdapter::with_tools(self, Arc<dyn OwnedProcessLaunch>) -> Self`, `with_engine_key(self, String) -> Self`; `resolve(declared: Engine, spec: AdapterSpec, tools: Option<Arc<dyn OwnedProcessLaunch>>) -> Result<Box<dyn EngineAdapter>, RuntimeError>`.

- [ ] **Step 1: Failing tests**

The stub engine: an `axum` server on `127.0.0.1:0` that serves `/v1/models` listing the served name after `ready_after` polls and `/v1/chat/completions` returning `{"choices":[{"message":{"content":"ready"}}]}` only when the `Authorization: Bearer <key>` header matches. The scripted tool: `ScriptedTool { identity, present: Mutex<Presence>, group: Vec<ProcessIdentity>, spawned: Mutex<Vec<RenderedCommand>> }` implementing the trait without touching processes.

```rust
/// Spec §4: render, spawn, readiness, probe, enumerate, observe. T10
#[tokio::test]
async fn initialize_spawns_waits_probes_and_reports_the_group() {
    let (stub, port) = stub_engine("gate-m", 2, "k3y").await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![worker0()]));
    let adapter = VllmAdapter::new(url(port), Some("k3y".into()), "fp".into(), ParkPolicy::Enabled, "gate-m".into())
        .with_launch(plan(port)).with_tools(tool.clone()).with_engine_key("k3y".into());
    let observation = adapter.execute_persisted(&initialize_command(port)).await.unwrap();
    let spawned = tool.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].env.get("VLLM_API_KEY").map(String::as_str), Some("k3y"));
    assert!(!spawned[0].argv.iter().any(|a| a == "k3y"));
    assert!(spawned[0].env.get("PATH").unwrap().starts_with("/opt/venv/bin:"));
    assert_eq!(observation.identities.iter().map(|i| i.role.as_str()).collect::<Vec<_>>(), ["api", "worker-0"]);
    assert_eq!(observation.facts, vec![Milestone::AllocationsRestored, Milestone::WeightsUsable, Milestone::CacheValid, Milestone::ModelUsable]);
}

/// Spec §4 step 4: a process that dies before readiness ends the step with the log tail.
#[tokio::test]
async fn a_process_gone_before_readiness_fails_with_the_log_tail() { /* tool.present -> Gone after spawn; log file has 3 lines; error message contains them */ }

/// Spec §4: the builder stops 2 s before the context deadline with the process alive.
#[tokio::test]
async fn a_deadline_with_the_process_alive_is_reported_as_such() { /* stub never ready; deadline = now + 3 s; error mentions "deadline" and "alive"; elapsed < 3 s */ }

/// One launch per incarnation.
#[tokio::test]
async fn a_second_initialize_for_the_same_incarnation_is_unsupported() {}

/// Spec §3: an unkeyed probe is refused by the engine and the step fails.
#[tokio::test]
async fn a_probe_without_the_key_does_not_pass() { /* adapter built with a different key than the stub → error */ }
```

- [ ] **Step 2: Run, watch fail. Step 3: Implement `initialize.rs`.**

```rust
//! The vLLM Initialize step (spec §4). The builder does the whole step: render,
//! spawn through the director's tool, wait for readiness while watching the
//! process, probe once, enumerate the group, and report identities and facts.
pub(super) async fn initialize(adapter: &VllmAdapter, command: &RuntimeCommand) -> Result<EffectObservation, RuntimeError> {
    let c = &command.context;
    let (plan, tools, key) = adapter.launch_parts()?; // Unsupported if any is missing
    if !matches!(c.launch_settings, Some(ProfileLaunchSettings::Vllm(_))) || !matches!(c.identities, ExecutionIdentities::OwnedLaunch) {
        return Err(RuntimeError::Unsupported);
    }
    adapter.claim_incarnation(&c.binding_id, &c.incarnation)?; // Unsupported on a repeat
    let mut cmd = render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    // Spec §3: the key rides the environment, never argv.
    cmd.env.insert("VLLM_API_KEY".into(), key.clone());
    if let Some(extra) = &plan.engine_path_extra { let sys = std::env::var("PATH").unwrap_or_default(); cmd.env.insert("PATH".into(), format!("{extra}:{sys}")); }
    if let Some(log) = &plan.engine_log { cmd.env.insert("MLLM_ENGINE_LOG".into(), log.clone()); }
    let incarnation = c.incarnation.clone();
    let spawn_tool = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tool.spawn_durable(&incarnation, &cmd)).await.map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // Spec §4: the builder ends 2 s before the coordinator's bound so its own error wins.
    let stop_at = c.deadline_ms - 2_000;
    loop {
        match adapter.check_readiness(&member(c)).await {
            Ok(Readiness::Ready) => break,
            Ok(_) => {}
            Err(e) => return Err(RuntimeError::Uncertain(format!("readiness: {e}"))),
        }
        let probe_tool = tools.clone(); let id = api.clone();
        match tokio::task::spawn_blocking(move || probe_tool.present(&id)).await.map_err(|_| RuntimeError::Uncertain("presence task failed".into()))? {
            Presence::Alive => {}
            Presence::Gone => return Err(RuntimeError::Uncertain(format!("engine exited before readiness; log tail:\n{}", log_tail(plan.engine_log.as_deref(), 20)))),
            Presence::Unknown => return Err(RuntimeError::Uncertain("engine presence could not be established during readiness".into())),
        }
        if now_ms()? >= stop_at { return Err(RuntimeError::Uncertain("readiness deadline reached with the engine alive".into())); }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let body = json!({"model": plan.served_model_name, "messages":[{"role":"user","content":"Say ready."}], "max_tokens": 8, "temperature": 0});
    let answer = adapter.forward_chat(&body).await.map_err(|e| RuntimeError::Uncertain(format!("engine listed the model but did not answer: {e}")))?;
    if answer["choices"][0]["message"]["content"].as_str().map_or(true, str::is_empty) {
        return Err(RuntimeError::Uncertain("engine answered with empty content".into()));
    }
    let group_tool = tools.clone(); let id = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tool.observe_group(&id)).await.map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    if identities.first().map(|i| i.role.as_str()) != Some("api") || identities.len() < 2 {
        return Err(RuntimeError::Uncertain(format!("engine group incomplete: {identities:?}")));
    }
    adapter.remember_launch(&c.binding_id, &c.incarnation, &api);
    Ok(EffectObservation {
        token: c.token.clone(), binding_id: c.binding_id.clone(), incarnation: c.incarnation.clone(),
        identities, observed_at_ms: now_ms()?,
        receipt: format!("vllm {} ready on {}; probe answered", adapter.fingerprint(), adapter.endpoint()),
        facts: vec![Milestone::AllocationsRestored, Milestone::WeightsUsable, Milestone::CacheValid, Milestone::ModelUsable],
    })
}
```

`log_tail(path, n)` reads the last `n` lines, bounded to 64 KiB, and passes them through `crate::vllm::args::redact_text` (Task 9 makes redaction shared; for now implement `redact_text` in `args.rs` beside `fingerprint_of` blanking `Bearer …`, `VLLM_API_KEY=…`, and any 40-plus character base64/hex run). `execute_persisted` for any other action returns `Unsupported` until S2. `now_ms()` uses `SystemTime`; a failure is `Uncertain`.

`resolve.rs`: `AdapterSpec::Vllm { endpoint, api_key, fingerprint, policy, model_id, launch: Option<PlanInputVllm>, engine_key: Option<String> }`; `resolve(declared, spec, tools)` attaches `launch`, `tools`, `engine_key` when present.

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-adapters --all-targets -- --test-threads=4`

```bash
git add crates/mllm-adapters
git commit -m "feat(adapters): the vLLM builder performs Initialize

Render, spawn through the durable tool with the key in the environment, poll
readiness while watching the process, stop two seconds before the deadline with
a clear reason, probe once, enumerate the group and report api plus workers with
the facts a cold start proves. One launch per incarnation."
```

---

### Task 9: Coordinator: builds the vLLM builder, times it right, fails it right, stops it for real

**Files:**
- Modify: `crates/mllm-controller/src/engine_bindings.rs` (plan, key, `ProfileBindings::new(clock, log_dir)`), `crates/mllm-controller/src/coordinator/worker.rs` (`CoordinatorOptions.initialize_timeout`, `terminate_grace`; `spawn_resolved` factory builds tools and association, cleanup closure; `drive` bound; failure branch)
- Create: `crates/mllm-controller/src/coordinator/native_failure.rs`
- Test: `crates/mllm-controller/src/coordinator/tests.rs` with a scripted `OwnedProcessLaunch` and the existing ordinary fixture

**Interfaces:**
- Consumes: Tasks 1 to 8.
- Produces: `CoordinatorOptions { initialize_timeout: Duration (default 900 s, 30 s..=7200 s), terminate_grace: Duration (default 15 s, 1 s <= grace, grace + 5 s < protocol_timeout), .. }`; `pub type ToolsFactory = Arc<dyn Fn(Arc<dyn LaunchAssociation>) -> Arc<dyn OwnedProcessLaunch> + Send + Sync>`; `OwnedCoordinator::spawn_resolved(owner, observations, clock, options, bindings, tools_factory: ToolsFactory)`; `ProfileBindings::new(clock, log_dir: PathBuf)`; `OwnedCoordinatorState::open_with_secrets(server_dir: &Path, key: SecretsKey) -> Result<Self, OwnedStateError>` (opens the store and calls `set_secrets_key`; `open` stays for tests and uses an ephemeral key); `Driver { engine, cleanup, tools: Option<Arc<dyn OwnedProcessLaunch>> }`.

- [ ] **Step 1: Failing tests**

```rust
/// Spec §4: Initialize is bounded by initialize_timeout, not protocol_timeout. T10
#[tokio::test]
async fn initialize_outlives_protocol_timeout() { /* options protocol_timeout 200 ms, initialize_timeout 5 s; scripted builder becomes ready after 1 s; deployment reaches Ready */ }

/// Spec §6: a native launch that fails after arm is terminated, proven gone, released
/// with evidence, journaled, and the deployment reads Closed. T20
#[tokio::test]
async fn a_failed_native_launch_is_released_and_closed() {
    // scripted tool: spawn ok, present -> Gone after first poll; builder returns Uncertain("engine exited")
    // assert: step cancelled, binding released, resource_owners empty, engine_secrets empty,
    //         journal has an entry naming the deployment and "engine exited",
    //         initialize_status reads Closed, worker status Running, another deployment starts fine
}

/// Spec §6 step 1: nothing recorded → released on the never-released outcome, no verify_gone.
#[tokio::test]
async fn a_launch_with_no_recorded_identity_is_released() { /* tool.spawn_durable returns Err before association */ }

/// Spec §6 step 4: unprovable stays Uncertain with the reservation retained.
#[tokio::test]
async fn an_unprovable_failure_pauses() { /* tool.terminate_owned -> Err; resource_owners still 1; status Uncertain */ }

/// Spec §5: Stop terminates the group and proves it gone before releasing. T12
#[tokio::test]
async fn stop_terminates_then_proves_gone() { /* after Ready, Stop; scripted tool records terminate_owned call; cleanup completes */ }

/// Spec §5: a grace that cannot fit the cleanup bound is refused at construction.
#[test]
fn terminate_grace_must_fit_protocol_timeout() { /* protocol_timeout 10 s, grace 8 s → Err(Invalid) */ }
```

- [ ] **Step 2: Run, watch fail. Step 3: Implement.**

`CoordinatorOptions`: add the two fields, defaults, validation in `spawn`.

`engine_bindings.rs`: `ProfileBindings { clock, log_dir }`. For `Engine::Vllm`, build `PlanInputVllm` per spec §3 table from `effective` and `work.endpoint()`; `engine_key`: generate with `mllm_store::secrets::new_engine_key()`, hex-encode, and return it in the spec; the factory (below) stores it through the owner before building the adapter. Policy: `profile.security.deep_park` → `ParkPolicy`. `sleep_flags`: `["--enable-sleep-mode", "--safetensors-load-strategy", "eager"]` when `launch_settings.enable_sleep_mode && deep_park == Enabled`, else empty (keep the flag list `live_vllm_sleep_flags` in `roles.rs` uses today; move it here). `engine_log = log_dir/<deployment>/<incarnation>.log`. `engine_path_extra = parent of profile.executable`. `--trust-remote-code` is already validated by config.

`spawn_resolved` factory closure: after `bindings.spec(work)`, if the spec carries an `engine_key`, `owner.store().store_engine_key(binding, incarnation, key)` under the owner lock; build the association:

```rust
struct StoreAssociation { owner: SharedCoordinatorState, fence: DeploymentFence, binding_id: String }
impl LaunchAssociation for StoreAssociation {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError> {
        let owner = self.owner.lock().map_err(|_| AssociationError::Uncertain("owner poisoned".into()))?;
        owner.store().record_api_identity(owner.session(), &self.fence, &self.binding_id, identity)
            .map_err(|e| AssociationError::Uncertain(e.to_string()))
    }
}
```

then `tools = tools_factory(Arc::new(association))`, `engine = resolve(declared, spec, Some(tools.clone()))`, and the cleanup closure for a builder with tools:

```rust
cleanup: Arc::new(move |context| {
    let tools = tools.clone(); let clock = clock.clone(); let grace = options.terminate_grace;
    Box::pin(async move {
        // Spec §5: terminate first, then the existing gone-proof. Blocking work off the async threads.
        let identities = context.identities.clone();
        tokio::task::spawn_blocking(move || tools.terminate_owned(&identities, grace))
            .await.map_err(|_| CoordinatorError::Service("terminate task failed".into()))?
            .map_err(|e| CoordinatorError::Service(e.to_string()))?;
        observed_gone(&context, &clock)
    })
}),
```

A builder without tools (Fake, until Task 13) keeps plain `observed_gone`.

`drive`: bound is `min(deadline - now, options.initialize_timeout)` for Initialize (cleanup keeps `protocol_timeout`).

Failure branch (`native_failure.rs`, called from the `InitializeStatus::Armed` arm in place of `mark_initialize_uncertain` when the driver has tools):

```rust
/// Spec §6: classify by proof. Terminate what was recorded, prove it gone, release
/// with evidence and close the deployment. Anything unprovable pauses Uncertain.
pub(super) async fn settle_failed_native_launch(shared: &Arc<Shared>, driver: &Driver, work: &InitializeWork, reason: &str) -> Result<InitializeStatus, CoordinatorError> {
    let recorded = shared.read(|owner, _| owner.store().runtime_binding_identities(&work.binding_id())).await?; // Vec<ProcessIdentity>
    let evidence = if recorded.is_empty() {
        CleanupEvidence { binding_id, incarnation, identities: vec![], observed_at_ms: now, receipt: "gate never opened; gated child disposed of by the launcher".into() }
    } else {
        let tools = driver.tools.clone().ok_or(...)?; let ids = recorded.clone(); let grace = shared.options.terminate_grace;
        match tokio::task::spawn_blocking(move || tools.terminate_owned(&ids, grace)).await? {
            Ok(()) => CleanupEvidence { identities: recorded, receipt: "every recorded process observed gone after termination".into(), .. },
            Err(e) => { mark_initialize_uncertain(...); journal(...); return Ok(InitializeStatus::Uncertain); }
        }
    };
    shared.read(move |owner, now| {
        let ttl = owner.store().observation_ttl_for_step(&step)?;
        owner.store().release_failed_launch(owner.session(), &step, &evidence, now, ttl)?;
        owner.store().record_journal(None, Some(&operation_id), Some("failed"), &redact(&format!("launch failed: {reason}")))?; // SPEC §17
        owner.store().set_admission_enabled(&deployment_id, false)?; // ADR 0011 decision 4
        Ok(())
    }).await?;
    Ok(InitializeStatus::Closed)
}
```

`Store::runtime_binding_identities(binding_id) -> Result<Vec<ProcessIdentity>, LifecycleError>` reads `identities_json`; add it in `lifecycle.rs`. `redact` reuses `mllm_adapters::vllm::args::redact_text`. The `Closed` outcome takes the existing give-up path (status `Closed`, worker continues).

- [ ] **Step 4: Run and commit**

Run: `cargo test --offline -p mllm-controller --all-targets -- --test-threads=4`

```bash
git add crates/mllm-controller crates/mllm-store
git commit -m "feat(controller): the coordinator directs a native builder

ProfileBindings builds the vLLM launch plan and a per-launch engine key from the
frozen profile; the driver factory stores the key, records the API identity through
the durable launcher's callback, and gives the builder its process tools. Initialize
is bounded by its own timeout, not the 30-second protocol timeout. A launch that
fails after arm is terminated, proven gone, released with evidence, journaled and
closed; an unprovable state still pauses. Stop terminates the group before the
gone-proof."
```

---

### Task 10: The router reaches the engine the coordinator launched

**Files:**
- Modify: `crates/mllm-controller/src/port.rs` (`runtime_endpoint`), `crates/mllm-controller/src/coordinator_port.rs` (impl), `crates/mllm-adapters/src/forward.rs` (public constructor), `crates/mllm-router/src/lib.rs`, `crates/mllm-router/src/chat.rs`
- Create: `crates/mllm-router/src/forwarders.rs`
- Test: `crates/mllm-router/tests/router_core.rs` (existing tests adapt; add one)

**Interfaces:**
- Produces: `LifecyclePort::runtime_endpoint(&self, deployment: &str) -> Result<Option<RuntimeEndpoint>, LifecycleFault>` with `RuntimeEndpoint { endpoint: String, served_model: String, engine_key: Option<String>, incarnation: String }`; `mllm_adapters::forward::engine_forwarder(base: Url, model: String, key: Option<String>) -> Arc<dyn ChatForward>`; `RouterDeps.forwards: Arc<dyn ForwarderSource>` with `trait ForwarderSource { fn forwarder(&self, deployment: &str) -> Result<Arc<dyn ChatForward>, ForwarderError>; }`.

- [ ] **Step 1: Failing test**

```rust
/// Spec §3: the router resolves endpoint and key per deployment at request time. T19
#[tokio::test]
async fn the_router_forwards_to_the_deployments_live_endpoint_with_its_key() {
    // A LifecyclePort stub whose runtime_endpoint returns the stub engine's port and key "k3y";
    // stub engine requires the bearer; chat through the router succeeds; then the stub rotates
    // to incarnation 2 with key "k4y" and the next request uses it.
}
```

- [ ] **Step 2: Implement**

`coordinator_port.rs`: read `runtime_binding(deployment)` for endpoint and incarnation, `engine_key(binding_id, incarnation)` decrypted and hex-encoded, served model from the effective revision's first route. `forwarders.rs`: `LiveForwarders { port: Arc<dyn LifecyclePort>, cache: Mutex<HashMap<(String, String), Arc<dyn ChatForward>>> }` keyed by deployment and incarnation. Replace the `HashMap<String, Arc<dyn ChatForward>>` in `RouterDeps` and both lookups (`chat.rs:82`, `lib.rs:130`). The forwarder's allowed upstream paths are exactly `/v1/models` and `/v1/chat/completions`; `engine_forwarder` refuses any other, and a unit test pins it (L3 drives this).

- [ ] **Step 3: Run and commit**

Run: `cargo test --offline -p mllm-router -p mllm-controller -p mllm-adapters --all-targets -- --test-threads=4`

```bash
git add crates
git commit -m "feat(router): forward to the endpoint and key the coordinator recorded

The boot-time forwarding table could not reach an engine on a leased port with a
per-launch key. The lifecycle port now projects a deployment's live endpoint,
served name and key, and the router builds its forwarder from that per incarnation.
The forwarder forwards only the chat and models paths."
```

---

### Task 11: Standalone from the environment, provider seam, no more discarded adapter

**Files:**
- Modify: `crates/mllm-cli/src/standalone_config.rs`, `crates/mllm-cli/src/roles.rs`; Delete: `crates/mllm-cli/tests/live_spark.rs`
- Test: `crates/mllm-cli/src/standalone_config/tests.rs`, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Produces: `pub struct EngineInstallation { engine: Engine, executable: PathBuf, build_fingerprint: String, launch_settings: serde_json::Value, deep_park: bool, trust_remote_code: bool, models_root: PathBuf, runtime_dir: PathBuf }` (standalone: `MLLM_RUNTIME_DIR`, default `<repo>/runtime` resolved from `CARGO_MANIFEST_DIR` at build time for the checkout case, refused if `mllm_vllm_guard.py` is not in it); `pub trait EngineProvider: Send + Sync { fn installation(&self) -> Result<EngineInstallation, StartError>; fn bindings(&self, clock, log_dir) -> Arc<dyn EngineBindings>; fn tools_factory(&self) -> ToolsFactory; }`; `roles::start_standalone(state_dir)` builds `EnvEngineProvider` from `MLLM_VLLM_BIN`, `MLLM_MODELS_ROOT`, `MLLM_KV_CACHE_BYTES`, `MLLM_ENGINE_ARGS`, `MLLM_ENGINE_FINGERPRINT`, `MLLM_DEEP_PARK`, `MLLM_TRUST_REMOTE_CODE`, or returns `StartError::NoEngineInstallation`; `roles::start_standalone_with(state_dir, Arc<dyn EngineProvider>)`; `App::deploy(name, source: ModelSource)`.

- [ ] **Step 1: Failing tests**

```rust
/// Spec §8: no engine installation, no boot. The error names what is expected.
#[tokio::test]
async fn standalone_refuses_to_boot_without_an_engine_installation() {
    std::env::remove_var("MLLM_VLLM_BIN");
    let err = roles::start_standalone(dir.path()).await.err().unwrap();
    assert!(matches!(err, StartError::NoEngineInstallation(_)));
    assert!(err.to_string().contains("MLLM_VLLM_BIN") && err.to_string().contains("MLLM_MODELS_ROOT"));
}
/// Spec §7: the host policy carries the full vLLM launch settings, the model store,
/// deep_park enabled by default, and derives the engine's bin directory.
#[test]
fn host_policy_from_env_is_complete() { /* build EnvEngineProvider from a fake vllm at <tmp>/venv/bin/vllm and models root; host_policy resolves with resolve_effective against a deployment document; launch_settings.tensor_parallel_size == 1, requested_budget.kv_cache_bytes == 16 GiB, security.deep_park == enabled, model_store.path == root */ }
```

- [ ] **Step 2: Implement**

`standalone_config::host_policy(installation: &EngineInstallation, build_fingerprint, capacity)` emits the spec §7 table. `deployment_document(name, route, source: &ModelSource, capacity)` writes `model.source`. `roles.rs`: delete `LiveVllmProfile`, the discarded adapter/launcher construction, `live_vllm_sleep_flags` (moved to `engine_bindings.rs` in Task 9), and the Fake fallback branch; `start_standalone_inner(state_dir, provider)` loads `SecretsKey::load_or_create(state_dir/identity/secrets.key)`, opens the owner state with it (`OwnedCoordinatorState::open_with_secrets`), builds `RouterDeps` with `LiveForwarders`, constructs `CoordinatorOptions { initialize_timeout: 900 s, terminate_grace: 15 s, ..Default::default() }` explicitly, and passes `provider.bindings(clock, state_dir.join("logs"))` and `provider.tools_factory()` to `spawn_resolved`. The build fingerprint: `MLLM_ENGINE_FINGERPRINT` or the trimmed output of `<bin> --version` with a 20 s timeout; failure is `NoEngineInstallation` naming the path. Existing CLI tests that booted the Fake (a1_gate, standalone_lifecycle, standalone_start, stop_intent, roles_f1) switch to `start_standalone_with(dir, testkit_provider())` in Task 13; until then they use a temporary `mllm_adapters::fake`-backed provider defined in `roles.rs` under `#[cfg(test)]`, and the integration tests use a `fake_provider()` helper in `crates/mllm-cli/tests/support.rs`.

- [ ] **Step 3: Run and commit**

Run: `cargo test --offline -p mllm-cli --all-targets -- --test-threads=4` (runs the excluded `live_interactive.rs` as collateral; expected)

```bash
git add -A crates/mllm-cli
git commit -m "feat(cli): standalone boots a real engine installation or refuses

The engine installation comes from the environment with the full vLLM launch
settings, the model store, deep park on by default and the engine's bin directory
derived from its executable. Without one, standalone refuses with the variables it
expects. The env-driven adapter that was built and discarded is gone, and the
engine provider is a parameter so tests inject their own."
```

---

### Task 12: Deep-park switch honored end to end, T21 tests, redaction shared

Small consistency task after Tasks 6 to 11 land.

**Files:**
- Modify: `crates/mllm-controller/src/engine_bindings.rs` (already maps `deep_park`; add the disabled → no sleep flags rule and a test), `crates/mllm-adapters/src/vllm/args.rs` (`redact_text` covers `VLLM_API_KEY=`, `Bearer `, 40-plus hex or base64 runs), `crates/mllm-adapters/tests/vllm_adapter.rs` (T21 pair from Task 1 asserts through `ProfileBindings` too)

- [ ] **Step 1: Failing tests**

```rust
/// Spec §3: deep_park disabled launches without sleep mode and dev mode off. T21
#[test]
fn a_disabled_profile_launches_without_sleep_flags() { /* ProfileBindings::spec on a profile with deep_park disabled → spec.launch.sleep_flags empty; render → VLLM_SERVER_DEV_MODE=0; policy Disabled */ }
#[test]
fn an_enabled_profile_launches_with_sleep_mode_by_default() { /* sleep_flags non-empty, VLLM_SERVER_DEV_MODE=1, policy Enabled */ }
#[test]
fn redaction_blanks_keys_in_log_tails() { /* "VLLM_API_KEY=abc Bearer xyz 0123…(64 hex)" → all three blanked */ }
```

- [ ] **Step 2: Implement, run, commit**

Run: `cargo test --offline -p mllm-adapters -p mllm-controller --all-targets -- --test-threads=4`

```bash
git add crates
git commit -m "feat: one deep-park switch, and redaction for what the engine prints

A profile that disables deep park launches without sleep mode and with vLLM's
development mode off; the default launches ready to park. Log tails pass the same
redaction as recorded commands before they reach the journal."
```

---

### Task 13: The Fake engine leaves the product

**Files:**
- Create: `crates/mllm-testkit/` (`Cargo.toml`, `src/lib.rs`, `src/fake_engine.rs`, `src/fake_launcher.rs`, `src/lifecycle.rs`, `src/fixture.rs` from `crates/mllm-controller/tests/support/fixture.rs`, `src/provider.rs` implementing `EngineProvider` with a Fake installation)
- Delete: `crates/mllm-adapters/src/fake/`, `mllm_agent::Host` (`crates/mllm-agent/src/lib.rs` keeps `doctor` and `memory`), `OwnedCoordinator::spawn_fake`, `embedded_fake`/`with_embedded_fake`/`fake_engine` in `crates/mllm-controller/src/operations.rs`, `Engine::Fake` and `RawLaunchSettings::Fake`/`ProfileLaunchSettings::Fake` in `mllm-config` and `mllm-domain`, `AdapterSpec::Fake`
- Modify: `Cargo.toml` (workspace member), every test that used the Fake (list from `grep -rln "mllm_adapters::fake" crates tests`), each crate's `[dev-dependencies]` gains `mllm-testkit = { path = "../mllm-testkit" }`
- Test: a build check

**Interfaces:**
- Produces: `mllm_testkit::{FakeEngine, FakeLauncher, FakeFault, fake_provider(), fixture::*, ScriptedTool}`; `mllm_testkit::fake_provider()` implements `mllm_cli::roles::EngineProvider`? No: `mllm-testkit` must not depend on `mllm-cli` (cycle through dev-deps is allowed, but keep the seam in `mllm-controller`): the provider trait moves to `crates/mllm-controller/src/engine_provider.rs` in this task, and `mllm-cli` re-exports it.

- [ ] **Step 1: The build check that defines done**

Add `scripts/check-release-clean.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cargo build --release --bin mllm --offline
if strings target/release/mllm | grep -qE "FakeEngine|mllm_testkit|FakeLauncher"; then
  echo "release binary contains test artifacts" >&2; exit 1
fi
cargo tree -p mllm-cli -e normal --offline | grep -q "mllm-testkit" && { echo "mllm-testkit is a normal dependency" >&2; exit 1; }
echo "release binary clean"
```

Run it now: it fails (Fake symbols present).

- [ ] **Step 2: Create the crate and move**

`crates/mllm-testkit/Cargo.toml`: `publish = false`, dependencies on `mllm-adapters`, `mllm-domain`, `mllm-config`, `mllm-store`, `mllm-controller`, `tokio`, `async-trait`, `serde_json`, `tempfile`. `git mv` the three fake files into `src/`, rename modules, fix paths. Test deployments in the fixture use `engine: vllm` with a full vLLM `launch_settings` block and the injected Fake builder; `fake_provider()` returns an `EngineInstallation` with `executable: /bin/true`, fingerprint `fake-v1`, models root a temp dir, and a `bindings` that returns the Fake adapter for every work item and a `tools_factory` returning `ScriptedTool` (from Task 8's test, moved here).

Remove the `Fake` variants from config and domain and fix every match the compiler names. `mllm-agent` loses `Host` and its `mllm-adapters` fake import; keep `doctor` and `memory`. `operations.rs` loses the embedded fake; its tests that need an engine take one as a parameter from the testkit (`park_flow.rs`, `roles_f1.rs` and siblings move to `mllm_testkit::FakeEngine`).

- [ ] **Step 3: Run everything**

Run: `bash scripts/check-release-clean.sh` — expected: `release binary clean`.
Run: `cargo test --offline -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness -p mllm-cli -p mllm-router -p mllm-agent -p mllm-config -p mllm-domain -p mllm-testkit --all-targets --no-fail-fast -- --test-threads=4`
Expected: pass.
Run: `cargo clippy --offline --workspace --all-targets -- -D warnings`

- [ ] **Step 4: Commit**

```bash
git add -A Cargo.toml Cargo.lock crates scripts/check-release-clean.sh
git commit -m "refactor: the Fake engine is a test fixture, not a product

The Fake engine, fake launcher and lifecycle simulation move to mllm-testkit, which
no product crate depends on. The fake engine family leaves config and domain, the
embedded fake host leaves the agent, spawn_fake leaves the coordinator and the
embedded fake leaves the legacy controller. Tests inject the Fake through the engine
provider and driver factory seams. scripts/check-release-clean.sh proves the shipped
binary carries none of it."
```

---

### Task 14: Docs that S1 changes

**Files:**
- Modify: `docs/design/adr/0011-the-state-machine-owns-recovery.md` (decision 5 row 3 per spec §6; "does not decide" paragraph on explicit Stop removed), `docs/SPEC.md` (§9.1 and §20 T21: add one sentence each pointing at the owner's 2026-09-17 decision that deep parking is on by default with a host opt-out, to be amended in full by the S2 ADR), `AGENTS.md` (the deep-park hard constraint gains the same pointer), `docs/runbooks/f2-current-status.md` (S1 entry under A1b, open items: post-launch retry deferred, vLLM control-route keying result from Task 15)

- [ ] **Step 1: Edit, then grep**

Run: `grep -n "explicit Stop" docs/design/adr/0011-the-state-machine-owns-recovery.md` — expected: none after the edit.

- [ ] **Step 2: Commit**

```bash
git add docs AGENTS.md
git commit -m "docs: ADR 0011 row 3 restored for native launches; deep-park default pointers

A launch that fails after arm is terminated, proven gone, released with evidence
and terminal for that start. SPEC §9.1, T21 and AGENTS.md point at the owner's
decision that deep parking is on by default with a host opt-out, pending the S2 ADR
that amends them in full."
```

---

### Task 15: The live run on host-a

**Files:**
- Create: `crates/mllm-cli/tests/live_vllm.rs`, `scripts/live/run-on-spark.sh`, `docs/runbooks/spark-live-f2.md`

**Interfaces:**
- Consumes: everything above. Env: `MLLM_LIVE=1`, `MLLM_VLLM_BIN=$HOME/mllm-vllm-venv2/bin/vllm`, `MLLM_MODELS_ROOT=$HOME/models`, `PROTOC=$HOME/.local/bin/protoc`.

- [ ] **Step 1: The scenarios** (spec §9, L1 to L11; each a `#[tokio::test]` gated on `MLLM_LIVE`, run with `--test-threads=1`)

```rust
fn live() -> bool { std::env::var("MLLM_LIVE").as_deref() == Ok("1") }
fn state_dir() -> tempfile::TempDir { tempfile::TempDir::new_in(std::env::var("HOME").unwrap()).unwrap() } // owner-only root; /tmp is refused by the controller lock

// T10  L1+L2+L3+L4+L5 in one ordered test so the engine is started once for the happy path:
#[tokio::test] async fn l1_to_l5_cycle() {
    if !live() { return; }
    let dir = state_dir(); let app = roles::start_standalone(dir.path()).await.unwrap();
    let id = app.deploy("qwen3-4b", ModelSource::Local { path: "qwen3-4b-instruct".into() }).unwrap();
    let t0 = Instant::now(); let op = app.controller.request_transition(&id, LifecycleAction::Start).await.unwrap();
    assert_eq!(app.controller.wait_terminal(&op).await.unwrap(), LifecycleState::Ready); record("L1 ready_secs", t0.elapsed());
    let ids = app.controller.live_identities(&id).unwrap(); assert!(ids.iter().any(|i| i.role == "api") && ids.iter().any(|i| i.role == "worker-0"));
    let argv = engine_argv(ids[0].pid); // std::fs::read_to_string(format!("/proc/{pid}/cmdline")) with NULs replaced by spaces
    for flag in ["--host", "--served-model-name", "--tensor-parallel-size", "--kv-cache-dtype", "--block-size"] { assert!(argv.contains(flag), "{flag} missing: {argv}"); }
    // L2: router chat, plain and streaming, with the user key (as a1_gate.rs does)
    // L3: no user key → 401/403; `ss -ltnp` shows 127.0.0.1:<port> only; TcpStream::connect(<routable ip>:<port>) is refused;
    //     direct http://127.0.0.1:<port>/v1/models without bearer → 401; direct POST /sleep and /collective_rpc without bearer → 401 (mllm guard), GET /is_sleeping with bearer → 200;
    //     forwarder refuses "/metrics" upstream (unit-level call on LiveForwarders).
    // L4: Stop → wait; no pid from `ids` alive; `pgrep -f "vllm serve"` empty; endpoint_leases and resource_owners empty; observed_state stopped
    // L5: Start again → Ready; new incarnation, new pid, engine_secrets row differs
}
// T20  L6 bad model dir → Closed; journal has reason + redacted tail; L7 fix source → Ready + answer
#[tokio::test] async fn l6_l7_failure_and_recovery() {}
// T20  L8 /bin/false as engine with MLLM_ENGINE_FINGERPRINT set → Closed not Uncertain; no leftover `sh` in the group
#[tokio::test] async fn l8_executable_that_exits_at_once() {}
// T20  L9 healthy model, deployment request_deadline 20 s → engine alive at deadline → terminated, gone, Closed
#[tokio::test] async fn l9_readiness_deadline() {}
// L10 release binary: scripts/check-release-clean.sh result is asserted by the runner, and standalone without MLLM_VLLM_BIN → NoEngineInstallation
#[tokio::test] async fn l10_no_engine_no_boot() {}
// L11 /proc/meminfo MemAvailable before Start, at Ready, after Stop; assert after-Stop within 2 GiB of before
#[tokio::test] async fn l11_memory_returns() {}
```

Timings and samples are appended to `target/live/current/results.md` by a `record()` helper; the runner copies it into the evidence runbook entry.

- [ ] **Step 2: The runner**

`scripts/live/run-on-spark.sh`:

```bash
#!/usr/bin/env bash
# Live run of S1 on the only authorized host (AGENTS.md). Refuses anything else.
set -euo pipefail
HOST=host-a
[ "${1:-$HOST}" = "$HOST" ] || { echo "only $HOST is authorized" >&2; exit 2; }
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
ssh -o BatchMode=yes "$HOST" 'if pgrep -af "sglang.launch_server|vllm serve|EngineCore" ; then echo "another engine is on the box; refusing" >&2; exit 3; fi'
rsync -az --delete --exclude target --exclude .git ./ "$HOST:~/mllm-f2/"
ssh -o BatchMode=yes "$HOST" bash -s <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH PROTOC=$HOME/.local/bin/protoc
cd ~/mllm-f2
cargo build --release --bin mllm
bash scripts/check-release-clean.sh
cargo build --release --tests -p mllm-cli
export MLLM_LIVE=1 MLLM_VLLM_BIN=$HOME/mllm-vllm-venv2/bin/vllm MLLM_MODELS_ROOT=$HOME/models
mkdir -p target/live/current
cargo test --release -p mllm-cli --test live_vllm -- --test-threads=1 --nocapture 2>&1 | tee target/live/current/test-output.log
REMOTE
mkdir -p "target/live/$STAMP"
rsync -az "$HOST:~/mllm-f2/target/live/current/" "target/live/$STAMP/"
echo "evidence under target/live/$STAMP"
```

Nothing is killed by name. On a refusal the script prints what it found and exits.

- [ ] **Step 3: The fact, already established**

Verified on the box on 2026-09-17: vLLM 0.29's `AuthenticationMiddleware` has
`GUARDED_PREFIX = ("/v1", "/v2", "/inference", "/cohere")`; the development routes are
unauthenticated. Task 7a's guard closes that. Record the finding and the guard in the
first evidence entry, and keep L3's direct-route assertions as written above.

- [ ] **Step 4: Run, then record**

Run: `bash scripts/live/run-on-spark.sh`
Expected: eleven scenarios green. On any failure, fix on the branch, re-run; every run appends to `docs/runbooks/spark-live-f2.md`:

```markdown
## 2026-MM-DD — S1 run N — <commit>
vLLM 0.29.0, qwen3-4b-instruct, host-a (unified-memory host, 121 GiB unified). Command: scripts/live/run-on-spark.sh
| Scenario | Result | Timing / sample |
| L1 | pass | cold start to Ready: NN.N s |
...
Failures and what changed: …
vLLM control routes keyed by API key: yes/no (see Step 3).
```

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli/tests/live_vllm.rs scripts/live/run-on-spark.sh docs/runbooks/spark-live-f2.md
git commit -m "test: S1 live run on host-a

Eleven scenarios drive the product path against vLLM 0.29 on the host: launch,
serve, access control, stop with a proven-empty group, restart, three failure
shapes, recovery, the clean release binary, and memory returned. The runner refuses
any host but host-a, refuses to start beside another engine, kills nothing by
name, and copies evidence back. Results are recorded in the evidence runbook."
```

---

### Task 16: Verify and record

- [ ] **Step 1: Core suite and clippy**

Run: `cargo test --offline -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`
Run: `cargo test --offline -p mllm-config -p mllm-cli -p mllm-router -p mllm-domain -p mllm-agent -p mllm-launchers -p mllm-scheduler -p mllm-protocol -p mllm-testkit --all-targets -- --test-threads=4`
Run: `cargo clippy --offline --workspace --all-targets -- -D warnings`
Run: `bash scripts/check-release-clean.sh`
Run: `grep -rhoE "// T[0-9]+" crates --include=*.rs | sort | uniq -c` — expected: T10, T12, T19, T20, T21 present alongside T15.

- [ ] **Step 2: Runbook**

`docs/runbooks/f2-current-status.md`: S1 entry under A1b with the live results summary and a link to `spark-live-f2.md`; open items: post-launch retry (spec §6), vLLM control-route keying (Task 15 Step 3 result), S1r next. Every status claim says CPU and Fake tests are a pre-check.

- [ ] **Step 3: Commit**

```bash
git add docs/runbooks/f2-current-status.md
git commit -m "docs: record S1, vLLM launches through the coordinator on host-a"
```

---

## Spec coverage

| Spec | Task |
|---|---|
| §2 pattern | 8, 9 |
| §3 trait, Presence, DurableProcessLaunch, log file, disposal | 1, 2 |
| §3 plan table, owned flags, five settings, no key on argv | 7, 9 |
| §3 park switch, trust-remote-code | 6, 12 |
| §3 control-route guard | 7a, 7, 15 |
| §3 engine key, encryption, deletion | 4, 9 |
| §3 router lookup, forwarder allowlist | 10 |
| §3 record_launch precondition; §4 worker rule | 3 |
| §3 requested budget bound | 6 |
| §4 Initialize step, margin | 8, 9 |
| §5 terminate, grace, closure selection, S1r obligations | 2, 9 |
| §6 failure path, release transition, redaction, way back | 5, 9, 12, 15 (L6, L7) |
| §7 installation env, model store, model source, fingerprint | 6, 11 |
| §8 Fake leaves, provider seam, ParkPolicy, ordering | 1, 11, 13 |
| §9 L1 to L11, runner, evidence | 15 |
| §10 done | 15, 16 |
| §11 ADR 0011 row 3, deep-park pointers | 14 |

## Deliberately left undone

Slice S1r (re-attach), S2 (park), post-launch retry, and the full SPEC §9.1 amendment, all named in the spec. The vLLM control-route keying question is answered in Task 15 and recorded, not fixed, in S1.
