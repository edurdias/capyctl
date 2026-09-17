# State Machine Owns Recovery Implementation Plan

**Goal:** A failed deployment stops itself rather than the host, and the state machine retries it three times before giving up.

**Architecture:** Three independent changes. The qualification gate is deleted from the ordinary lifecycle, so a parking deployment gets the same declared identity a restart-only one already gets. A failed step closes that deployment's admission instead of the coordinator's, and the worker keeps running. An attempt budget keyed to the deployment's fence drives retry with a doubling cooldown, and distinguishes a failed effect (retry) from an uncertain one (prove the processes gone first).

**Tech Stack:** Rust. `mllm-store` (schema, binding identity, attempt budget), `mllm-controller` (the coordinator worker loop).

**Spec:** `docs/design/adr/0011-the-state-machine-owns-recovery.md`

## Global Constraints

- Prose in code comments, commit messages and documents is normal English.
- Cite the governing requirement inline where behaviour is spec-driven: `// SPEC §13.2: ...` or `// ADR 0011 decision N: ...`. Never cite a plan task — it does not exist for a future reader.
- Do NOT read, edit, format or stage `crates/mllm-cli/tests/live_interactive.rs`, and do not touch anything under `.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/`. (`cargo test -p mllm-cli` runs that file as collateral of the target; that is expected and fine. Never open or modify it.)
- Use `cargo` with `--offline`.
- Core suite: `cargo test --offline -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`
- Clippy must pass with `-D warnings`.
- `qualification_progression::ordinary_cleanup_races_ready_completion_and_duplicate_accept_and_arm` fails roughly half the time with `Sql(DatabaseBusy)` at `ordinary_cleanup.rs:820`, on an idle machine, and did so before this work. Ignore that specific failure. Any **other** failure in that target is new and must be reported.
- This plan does not implement ordinary park. ADR 0011 removes park's gate and defines how failures are handled; the transition itself is later work.

---

### Task 1: Delete the qualification gate from the ordinary lifecycle

ADR 0011 decision 1. `binding_identity` currently demands a qualification catalog entry from any deployment that parks, which no real engine can satisfy and which nothing consults when parking. A declared identity carries the same recipe and host fingerprints.

**Files:**
- Modify: `crates/mllm-store/src/ordinary_lifecycle.rs` — `BindingIdentity` (around 417-447), `DeclaredBindingV1` (around 422-432), `binding_identity` (around 450-475), and the `qualified_effective` import at line 8
- Test: `crates/mllm-store/tests/acceptance.rs` — the store's integration tests live in `tests/{acceptance,resource_transactions,snapshot}.rs`; `ordinary_lifecycle` has no inline test module

**Interfaces:**
- Consumes: nothing.
- Produces: `binding_identity` returns `BindingIdentity::Declared` for every residency. Tasks 2-4 do not depend on this.

- [ ] **Step 1: Write the failing test**

A parking deployment must get a binding without any catalog entry. Find how the store's existing tests build an effective deployment — `crates/mllm-config/tests/fixtures/f2-deployment.json` is the shared fixture and `resolve_effective` produces the type. Write a test that resolves a `deep` deployment on a host with `memory: distinct`, calls whatever path reaches `binding_identity`, and asserts it succeeds with an id beginning `declared:`.

```rust
/// ADR 0011 decision 1: a parking deployment is identified by its recipe and host,
/// exactly as a restart-only one is. It used to require a qualification catalog
/// entry, which no real engine could produce and which nothing read when parking.
#[test]
fn a_parking_deployment_gets_a_declared_identity() {
    // Build a `deep` effective deployment on a host whose domain memory is
    // `distinct`, so ADR 0010's host check admits it.
    // Then drive the path that reaches binding_identity and assert:
    //   - it succeeds with no qualification catalog row present
    //   - the binding id starts with "declared:"
    //   - the payload records residency "deep", not "restart_only"
}
```

Fill the body using the same construction the neighbouring tests in `acceptance.rs` use. `binding_identity` is private, so reach it through `Store::accept_qualified_start`, which is the ordinary path that calls it. If `acceptance.rs` has no fixture that builds an effective deployment, `crates/mllm-controller/tests/qualification_support/fixture.rs` shows how one is assembled — read it for the shape, but keep your test in `mllm-store` and build the fixture locally rather than depending on the controller crate.

- [ ] **Step 2: Run it and watch it fail**

Expected: failure from `qualified_effective`, either `LifecycleError::Unsupported` (the engine is not Fake) or `LifecycleError::Conflict` (no catalog row).

- [ ] **Step 3: Make the identity declared for every residency**

In `crates/mllm-store/src/ordinary_lifecycle.rs`, `DeclaredBindingV1`'s `residency` field becomes an owned `String` so it can carry the real value:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredBindingV1 {
    version: u8,
    kind: &'static str,
    residency: String,
    recipe_fingerprint: String,
    host: String,
    hardware_fingerprint: String,
    environment_fingerprint: String,
}
```

`binding_identity` loses its branch entirely:

```rust
/// A runtime's identity, derived from what it was admitted against.
///
/// ADR 0011 decision 1: every residency is identified this way. A changed recipe or
/// a moved host yields a different identity, so a binding cannot be silently reused
/// across either — which is the only property the lifecycle needs from an identity.
/// Parking used to require a qualification catalog entry instead; nothing read it
/// when parking, and what it supplied was this same recipe and host information.
fn binding_identity(
    tx: &Transaction<'_>,
    e: &mllm_config::effective::EffectiveDeployment,
    deployment: &str,
) -> Result<BindingIdentity, LifecycleError> {
    let _ = (tx, deployment);
    let descriptor = DeclaredBindingV1 {
        version: 1,
        kind: "declared",
        residency: residency_name(e.residency).to_string(),
        recipe_fingerprint: e.qualification_fingerprint.clone(),
        host: e.host.name.clone(),
        hardware_fingerprint: e.host.hardware_fingerprint.clone(),
        environment_fingerprint: e.host.environment_fingerprint.clone(),
    };
    Ok(BindingIdentity::Declared {
        id: format!("declared:{}", e.qualification_fingerprint),
        payload: encode(&descriptor)?,
    })
}

/// The serialized name of a residency, stable across refactors of the enum.
fn residency_name(residency: mllm_config::effective::Residency) -> &'static str {
    use mllm_config::effective::Residency;
    match residency {
        Residency::RestartOnly => "restart_only",
        Residency::HostBacked => "host_backed",
        Residency::Deep => "deep",
    }
}
```

If `tx` and `deployment` become genuinely unused after the change, drop them from the signature and fix the two call sites rather than keeping a `let _ =` — the placeholder above exists only so the diff compiles at this step.

Then delete the `Qualified` variant from `BindingIdentity`, its two match arms in `id()` and `payload()`, and the `use crate::candidate_creation::progression::catalog::qualified_effective;` import at line 8. The compiler will find anything else.

**Do not delete `qualified_effective` itself, the catalog, or the candidate machinery.** ADR 0011 decision 2 keeps them; they lose one caller.

- [ ] **Step 4: Run the test and the store suite**

Run: `cargo test --offline -p mllm-store`
Expected: your test passes. Other tests may fail because a fixture expected a qualified binding id — read each failure before changing it, and say in your report which fixtures moved and why.

- [ ] **Step 5: Run the controller suite**

Run: `cargo test --offline -p mllm-controller --all-targets -- --test-threads=4`
Expected: pass, apart from the documented `DatabaseBusy` flake.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-store
git commit -m "feat(store): a parking deployment needs no qualification

binding_identity demanded a qualification catalog entry from any deployment that
parks. No real engine could produce one, and nothing read the catalog when a
deployment parked or woke: the entry supplied an identity, and a declared binding
already derives the same recipe and host fingerprints.

Every residency is now identified the same way. The catalog and the candidate runs
that build it are unchanged and keep their tests; they lose one caller."
```

---

### Task 2: An attempt budget, keyed to the deployment's fence

ADR 0011 decision 5. Nothing counts attempts today. The budget is per deployment, revision and generation, so a new configuration starts fresh and a retry of the same one does not.

**Files:**
- Modify: `crates/mllm-store/src/schema.rs` (add `SCHEMA_V12`)
- Modify: `crates/mllm-store/src/migrations.rs` (register it, add a migration test)
- Create: `crates/mllm-store/src/attempts.rs`
- Modify: `crates/mllm-store/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: nothing.
- Produces, on `crate::Store`:
  - `pub fn record_attempt(&self, fence: &DeploymentFence, now_ms: i64) -> Result<AttemptRecord, StoreError>`
  - `pub fn attempts(&self, fence: &DeploymentFence) -> Result<Option<AttemptRecord>, StoreError>`
  - `pub struct AttemptRecord { pub attempts: i64, pub last_attempt_ms: i64 }`
  Task 4 calls both.

- [ ] **Step 1: Write the failing migration test**

Add to `crates/mllm-store/src/migrations.rs`'s test module:

```rust
/// An existing deployment keeps its rows and starts with no attempts recorded.
#[test]
fn v12_adds_attempts_and_preserves_rows() {
    let conn = Connection::open_in_memory().unwrap();
    for (index, sql) in MIGRATIONS.iter().take(MIGRATIONS.len() - 1).enumerate() {
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations(version) VALUES(?1)",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);").unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let name: String = conn
        .query_row("SELECT name FROM deployments WHERE id='kept'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "kept");
    let attempts: i64 = conn
        .query_row("SELECT COUNT(*) FROM deployment_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 0, "an upgrade records no attempts against anything");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --offline -p mllm-store --lib v12_adds_attempts`
Expected: `no such table: deployment_attempts`.

- [ ] **Step 3: Add the schema**

In `crates/mllm-store/src/schema.rs`:

```rust
/// ADR 0011 decision 5: a deployment's failed attempts are counted against the exact
/// configuration that failed. A new revision is a new configuration and starts fresh,
/// so the key is the fence rather than the deployment alone.
pub const SCHEMA_V12: &str = r#"
CREATE TABLE deployment_attempts(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision > 0),
  generation INTEGER NOT NULL CHECK(generation > 0),
  attempts INTEGER NOT NULL CHECK(attempts >= 0),
  last_attempt_ms INTEGER NOT NULL CHECK(last_attempt_ms >= 0),
  PRIMARY KEY(deployment_id, revision, generation)
);
"#;
```

Register it in `crates/mllm-store/src/migrations.rs`, in both the `use` list and `MIGRATIONS`, after `SCHEMA_V11`.

- [ ] **Step 4: Write the failing store test**

Create `crates/mllm-store/src/attempts.rs` with a test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DeploymentFence;

    fn fence(generation: i64) -> DeploymentFence {
        DeploymentFence {
            deployment_id: "dep".into(),
            revision: 1,
            generation,
        }
    }

    /// Attempts accumulate against one configuration and carry the time of the last
    /// one, which is what the cooldown is measured from.
    #[test]
    fn attempts_accumulate_and_carry_their_time() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('dep','dep','model','ready',1,0,1,1)", [])
            .unwrap();

        assert!(store.attempts(&fence(1)).unwrap().is_none(), "nothing yet");

        let first = store.record_attempt(&fence(1), 1_000).unwrap();
        assert_eq!(first.attempts, 1);
        assert_eq!(first.last_attempt_ms, 1_000);

        let second = store.record_attempt(&fence(1), 2_500).unwrap();
        assert_eq!(second.attempts, 2);
        assert_eq!(second.last_attempt_ms, 2_500);
    }

    /// A new generation is a new configuration: it does not inherit the failures of
    /// the one before it, or a deployment could never be restarted after exhausting
    /// its budget.
    #[test]
    fn a_new_generation_starts_fresh() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('dep','dep','model','ready',1,0,1,1)", [])
            .unwrap();
        store.record_attempt(&fence(1), 1_000).unwrap();
        store.record_attempt(&fence(1), 2_000).unwrap();

        let fresh = store.record_attempt(&fence(2), 3_000).unwrap();
        assert_eq!(fresh.attempts, 1, "generation 2 starts from zero");
        assert_eq!(
            store.attempts(&fence(1)).unwrap().unwrap().attempts,
            2,
            "generation 1's history is preserved"
        );
    }
}
```

- [ ] **Step 5: Run it and watch it fail**

Run: `cargo test --offline -p mllm-store --lib attempts`
Expected: compile failure, `no method named record_attempt`.

- [ ] **Step 6: Implement**

In `crates/mllm-store/src/attempts.rs`, above the test module:

```rust
//! How many times one configuration of a deployment has been attempted.
//!
//! ADR 0011 decision 5: the state machine retries a failed deployment and gives up
//! after a budget. The count is keyed to the fence, not the deployment, so a new
//! revision or generation is a fresh configuration with a fresh budget — otherwise a
//! deployment that exhausted its attempts could never be restarted.

use rusqlite::{params, OptionalExtension};

use crate::lifecycle::DeploymentFence;
use crate::StoreError;

/// Attempts recorded against one configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub attempts: i64,
    pub last_attempt_ms: i64,
}

impl crate::Store {
    /// Count one attempt and return the running total.
    pub fn record_attempt(
        &self,
        fence: &DeploymentFence,
        now_ms: i64,
    ) -> Result<AttemptRecord, StoreError> {
        if now_ms < 0 {
            return Err(StoreError::Conflict);
        }
        self.conn.execute(
            "INSERT INTO deployment_attempts(deployment_id,revision,generation,attempts,last_attempt_ms)
             VALUES(?1,?2,?3,1,?4)
             ON CONFLICT(deployment_id,revision,generation)
             DO UPDATE SET attempts = attempts + 1, last_attempt_ms = ?4",
            params![fence.deployment_id, fence.revision, fence.generation, now_ms],
        )?;
        self.attempts(fence)?.ok_or(StoreError::Conflict)
    }

    /// What has been recorded against this configuration, if anything.
    pub fn attempts(&self, fence: &DeploymentFence) -> Result<Option<AttemptRecord>, StoreError> {
        self.conn
            .query_row(
                "SELECT attempts,last_attempt_ms FROM deployment_attempts
                 WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
                params![fence.deployment_id, fence.revision, fence.generation],
                |row| {
                    Ok(AttemptRecord {
                        attempts: row.get(0)?,
                        last_attempt_ms: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests;
```

Keep the test module inline (`mod tests { ... }` as written in Step 4) rather than the trailing `mod tests;` line, or move the tests to `attempts/tests.rs` — pick whichever matches the neighbouring modules in this crate and be consistent.

Declare the module in `crates/mllm-store/src/lib.rs` beside the other `pub mod` lines.

- [ ] **Step 7: Run the store suite**

Run: `cargo test --offline -p mllm-store`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/mllm-store
git commit -m "feat(store): count a deployment's attempts against its own configuration

Nothing counted attempts, so the state machine had no basis for giving up. The
count is keyed to the deployment's fence rather than to the deployment, because a
new revision or generation is a different configuration: inheriting the failures of
the previous one would make a deployment that exhausted its budget impossible to
restart."
```

---

### Task 3: A failed step stops its deployment, not the host

ADR 0011 decision 4, and the most consequential change in this plan. When the worker loop returns any outcome that is not `Uncertain`, the surrounding task calls `close_admission()` (`crates/mllm-controller/src/coordinator/worker.rs:878`), which sets the coordinator's `accepting` and `cleanup_accepting` to false for every deployment, and the worker task ends. One deployment failing to initialize currently stops the whole host.

`deployments.admission_enabled` is already the per-deployment gate.

**Files:**
- Modify: `crates/mllm-controller/src/coordinator/worker.rs` — the initialize loop's failure branch (around 1393-1530) and the task epilogue (around 869-881)
- Modify: `crates/mllm-store/src/deployments.rs` — add a setter if none exists for `admission_enabled`
- Test: `crates/mllm-controller/tests/` — follow the structure of an existing coordinator test that drives an initialize to failure

**Interfaces:**
- Consumes: nothing from Tasks 1-2.
- Produces: the worker survives a failed step. Task 4 builds retry on top of that.

- [ ] **Step 1: Write the failing test**

The property: after one deployment's initialize fails, a second deployment can still be started, and the coordinator still accepts commands.

Existing tests that drive a worker to a `Failed` or `Blocked` outcome live in `crates/mllm-controller/src/coordinator/tests.rs`, `tests_cleanup.rs` and `tests_candidate.rs`. Read `tests_cleanup.rs` first: it has the closest fixture, driving an ordinary step to a terminal outcome. Model your test on it and put yours beside it.

```rust
/// ADR 0011 decision 4: a deployment that fails closes its own admission, not the
/// host's. Before this, the worker returned on any failed step and the task closed
/// admission for every deployment, so one bad configuration stopped everything.
#[tokio::test]
async fn a_failed_deployment_does_not_stop_the_others() {
    // Drive one deployment's initialize to a known failure.
    // Assert:
    //   - that deployment's admission_enabled is 0
    //   - the coordinator still accepts a command (it is not Stopped)
    //   - a second, healthy deployment starts and reaches Ready
}
```

- [ ] **Step 2: Run it and watch it fail**

Expected: the second start is refused with `CoordinatorError::Stopped("worker is not admitting Initialize")`, because global admission closed.

- [ ] **Step 3: Close the deployment's admission instead**

Add to `crates/mllm-store/src/deployments.rs` if no equivalent exists:

```rust
    /// Close or open one deployment's admission.
    ///
    /// ADR 0011 decision 4: a failed deployment stops itself. The coordinator's own
    /// admission is for shutdown, not for one deployment's bad configuration.
    pub fn set_admission_enabled(&self, id: &str, enabled: bool) -> Result<(), StoreError> {
        let updated = self.conn.execute(
            "UPDATE deployments SET admission_enabled = ?2 WHERE id = ?1",
            params![id, enabled as i64],
        )?;
        if updated == 0 {
            return Err(invalid_column("deployments"));
        }
        Ok(())
    }
```

In the worker's initialize failure branch, where the outcome is computed, a `WorkerStatus::Failed` or `Blocked` for a specific deployment must now:

1. close that deployment's admission through the store, and
2. `continue` the loop rather than `return`.

The existing epilogue condition is:

```rust
if !shared.accepting.load(Ordering::Acquire)
    || *stop.borrow()
    || !matches!(outcome, WorkerStatus::Uncertain { .. })
{
    return outcome;
}
```

Only shutdown should still return. A per-deployment failure continues. Keep the existing `Uncertain` pause exactly as it is — an uncertain outcome still holds the retained binding and waits for an explicit Stop, because nobody knows whether the effect landed.

Read the loop's control flow before editing; it holds a lock and a `paused` binding across iterations, and the `continue` must not skip the bookkeeping the `Uncertain` path relies on.

- [ ] **Step 4: Run the test**

Run: `cargo test --offline -p mllm-controller a_failed_deployment_does_not_stop_the_others`
Expected: pass.

- [ ] **Step 5: Run the whole controller suite**

Run: `cargo test --offline -p mllm-controller --all-targets -- --test-threads=4`
Expected: pass apart from the documented `DatabaseBusy` flake. Several existing tests assert the old behaviour — that a failure closes admission. Read each before changing it, and state in your report which assertions you changed and why the new behaviour is correct. If an existing test asserts that the coordinator stops on a failure that is genuinely fatal (a poisoned mutex, a corrupt store), leave it: those are not one deployment's failure.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-controller crates/mllm-store
git commit -m "feat(controller): a failed deployment stops itself, not the host

The worker returned from its loop on any outcome that was not uncertain, and the
task then closed admission for every deployment on the host. One deployment failing
to initialize stopped everything, which is a blast radius nobody chose.

A failure now closes that deployment's own admission and the loop continues. An
uncertain outcome is unchanged: it still holds the retained binding and waits for an
explicit Stop, because nobody knows whether the effect landed."
```

---

### Task 4: Retry, with a budget and a cooldown

ADR 0011 decision 5. Three attempts, 30s cooldown doubling.

**Files:**
- Modify: `crates/mllm-controller/src/coordinator/worker.rs` (the failure branch from Task 3)
- Modify: `crates/mllm-controller/src/coordinator.rs` or wherever `CoordinatorOptions` is defined — add the budget and cooldown
- Test: alongside Task 3's test

**Interfaces:**
- Consumes: `Store::record_attempt`, `Store::attempts`, `AttemptRecord` from Task 2; the surviving loop from Task 3.
- Produces: nothing further.

- [ ] **Step 1: Write the failing tests**

```rust
/// ADR 0011 decision 5: a failed attempt is retried, and the deployment is only
/// given up on after the budget is spent.
#[tokio::test]
async fn a_failed_start_is_retried_until_the_budget_is_spent() {
    // Drive a deployment whose initialize always fails.
    // Assert: exactly 3 attempts are recorded, then the deployment is terminally
    // failed with its admission closed, and no fourth attempt is made.
}

/// Retrying an effect that may have landed can start a second engine while the
/// first still holds memory. An uncertain attempt resolves through the gone-proof
/// before it is retried.
#[tokio::test]
async fn an_uncertain_attempt_is_not_retried_until_the_processes_are_gone() {
    // Drive a deployment's initialize to an uncertain outcome.
    // Assert: no attempt is recorded and no retry is made while the recorded
    // processes are still present.
}
```

Use the fake engine's fault injection to force each outcome — see `mllm_adapters::fake::QualificationFault` and `FakeEngine::with_qualification_fault` for the available modes, and the existing controller tests that use them.

- [ ] **Step 2: Run them and watch them fail**

Expected: the first fails because only one attempt is made; the second's shape depends on current behaviour — record what it does before you change anything.

- [ ] **Step 3: Add the policy to `CoordinatorOptions`**

```rust
    /// ADR 0011 decision 5: how many times one configuration is attempted before the
    /// deployment is given up on.
    pub max_attempts: u32,
    /// The wait before the first retry. Each subsequent retry doubles it, so a
    /// broken recipe does not burn a GPU in a tight loop while a transient failure
    /// does not wait minutes.
    pub retry_cooldown: Duration,
```

Defaults in the `Default` impl: `max_attempts: 3`, `retry_cooldown: Duration::from_secs(30)`.

Validate them where the other options are validated in `spawn_with_candidate_factory`: `max_attempts` between 1 and 16, `retry_cooldown` non-zero and at most an hour. Reject outside those, as the existing option checks do.

- [ ] **Step 4: Implement the retry**

In the failure branch from Task 3, for an outcome that is a **known failure**:

```rust
// SPEC §13.2 and ADR 0011 decision 5: a failure that is known not to have landed is
// retried. The attempt is counted against this exact configuration, and the
// deployment is given up on once the budget is spent.
let record = shared
    .read(move |owner, now| owner.store().record_attempt(&fence, now))
    .await?;
if record.attempts >= options.max_attempts as i64 {
    // close this deployment's admission; terminal.
} else {
    // wait cooldown * 2^(attempts - 1), then continue the loop to re-poll.
}
```

For an **uncertain** outcome, the existing pause stays. Retry only after the gone-proof establishes the processes are absent — that is `mllm_launchers::process_absence` and the `observed_gone` path the cleanup driver already uses. If the proof says `SomeAlive` or `Indeterminate`, do not retry and do not count an attempt.

Compute the cooldown with a saturating shift so a large attempt count cannot overflow.

- [ ] **Step 5: Run the tests**

Run: `cargo test --offline -p mllm-controller --all-targets -- --test-threads=4`
Expected: pass apart from the documented flake. Your tests must not sleep for the real cooldown — inject a short one through `CoordinatorOptions` rather than waiting 30 seconds.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-controller
git commit -m "feat(controller): retry a failed deployment, and give up after a budget

A deployment's failed configuration is attempted three times with a doubling
cooldown, then given up on with its own admission closed.

An uncertain outcome is not a failure and is not retried on the same terms:
retrying an effect that may have landed can start a second engine while the first
still holds memory, so the recorded processes must be proven gone first. A runtime
whose processes are still present is surfaced rather than guessed at."
```

---

### Task 5: Verify and record

**Files:**
- Modify: `docs/runbooks/f2-current-status.md`

- [ ] **Step 1: Core suite**

Run the core suite from Global Constraints. Expected: pass apart from the documented `DatabaseBusy` flake in `qualification_progression`.

- [ ] **Step 2: Remaining crates**

Run: `cargo test --offline -p mllm-config -p mllm-cli -p mllm-router -p mllm-domain -p mllm-agent -p mllm-launchers`

- [ ] **Step 3: Clippy**

Run: `cargo clippy --offline --workspace --all-targets -- -D warnings`

- [ ] **Step 4: Record it**

Under A1b in `docs/runbooks/f2-current-status.md`:

```markdown
- [x] Remove the qualification gate from parking, and give the state machine
      recovery (ADR 0011). A parking deployment is identified by its recipe and host
      like any other; a failed deployment closes its own admission rather than the
      host's; a failed configuration is retried three times with a doubling cooldown
      before it is given up on. An uncertain attempt still resolves through the
      gone-proof before any retry. This does not implement park.
```

- [ ] **Step 5: Commit**

```bash
git add docs/runbooks/f2-current-status.md
git commit -m "docs: record that the state machine now owns recovery"
```

---

## Spec decisions with no task, and why

ADR 0011 decision 3 says park is an ordinary transition with no gate the others lack.
Task 1 removes the gate, which is the whole of it — there is no park to give a gate to.

ADR 0011 decision 6 adopts `SPEC.md` §13.2's wake-failure recovery, including that a
bounded clean restart counts as one of the three attempts. There is no wake to fail
yet. Task 4 builds the budget that recovery will spend; the wake path spends it when
park exists.

## What this plan deliberately leaves undone

**Ordinary park.** ADR 0011 removes park's gate and defines how its failures are handled. The transition — drain, park, parked accounting, wake — is not designed yet and needs its own ADR.

**Eviction policy.** Which deployment to park when another needs memory is a scheduling question nobody has answered.

**Native parking.** `VllmAdapter` has no `execute_persisted`, and `ProfileBindings` refuses SGLang for a missing admin credential and a missing trusted observation socket. Both block parking on a real engine regardless of this plan.

**Operator recovery from terminal failure.** A deployment that spends its budget stays failed until a new revision resets the count. Whether an operator should be able to reset it without a revision is not decided.
