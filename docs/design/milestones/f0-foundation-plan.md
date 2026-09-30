# F0 Foundation Implementation Plan

**Goal:** Build the capyctl F0 foundation — an 11-crate Rust workspace with the lifecycle state machine, strict config, transactional SQLite store, resource ledger, engine-adapter/launcher contracts, fake-engine harness, and the action-first CLI — such that T01–T04, T08, T09, T26, T27 pass without GPUs and the exit gate runs a full fake-engine lifecycle.

**Architecture:** One binary, 11 workspace crates, hard module boundaries (`capyctl-domain` is pure; the fake adapter sits beside future real adapters behind one trait). Server + embedded host share contracts via in-process calls; the gRPC proto is frozen but exercised only by a wire-level round-trip test. All state lives in per-user SQLite under owner-only permissions.

**Tech Stack:** Rust (stable, edition 2021), tokio, tonic + prost (gRPC), rusqlite (bundled SQLite), saphyr-parser/yaml-rust2 (strict YAML), clap 4, serde, ulid, proptest, tempfile.

**Spec:** `docs/design/milestones/f0-foundation-design.md` (approved + reviewed) — the plan implements it; upstream authority is `docs/SPEC.md` rev 0.2. Executors read both.

## Global Constraints

- Language: Rust, stable toolchain; no Python server runtime. One binary (`capyctl`) for all roles.
- Persistence: embedded SQLite via `rusqlite` (feature `bundled`); server store + agent journal; forward-only migrations; no external broker.
- Transport: gRPC package `capyctl.management.v1`; field numbers per Appendix A of the design doc; additive evolution only.
- Config: versioned strict YAML; reject unknown capyctl fields, duplicate keys, invalid units, missing required fields; explicit missing/invalid config fails — generation only for implicit-missing cases.
- `auto` v1 constants: `managed_limit = min(75% × observed, observed − 8 GiB)`, `free_reserve = max(8 GiB, 10% × observed)`; observation TTL 60 s; clock-skew tolerance 30 s; all persisted with provenance.
- Idempotency key = `SHA-256(server context id, deployment name, canonical manifest bytes)`; key/content mismatch → conflict error.
- Permissions: state dirs 0700, files 0600, at creation.
- Exit codes: 0 ok, 2 invalid config, 3 unauthorized, 4 insufficient resources, 5 unsupported, 6 unreconciled, 7 device conflict, 8 category limit, 10 activation timeout, 11 topology unknown, 12 no safe estimate.
- Security gate: deep-park/collective-control ops denied by default; enabled only by explicit host-policy opt-in; conformance suite covers default denial.
- Store files never hold inference bodies; logs never hold prompts or secrets.
- Every task ends with `cargo test -w` green and a git commit on `design/initial`.

## File Structure

```text
Cargo.toml                    # workspace root, shared profile
rust-toolchain.toml           # channel = stable
crates/
  capyctl-domain/src/{lib.rs, lifecycle.rs, identity.rs, error.rs}
  capyctl-store/src/{lib.rs, schema.rs, deployments.rs, migrations.rs}
  capyctl-scheduler/src/{lib.rs, ledger.rs, admission.rs, auto.rs}
  capyctl-protocol/{proto/capyctl/management/v1/management.proto, build.rs, src/lib.rs}
  capyctl-config/src/{lib.rs, schema.rs, strict_yaml.rs, defaults.rs}
  capyctl-adapters/src/{lib.rs, traits.rs, fake/{mod.rs, engine.rs, launcher.rs}}
  capyctl-launchers/src/{lib.rs, exec.rs}
  capyctl-agent/src/{lib.rs, supervision.rs}
  capyctl-controller/src/{lib.rs, operations.rs}
  capyctl-router/src/lib.rs      # F0: placeholder module (F1 fills it)
  capyctl-cli/src/{main.rs, grammar.rs, output.rs, roles.rs}
tests/harness/src/{lib.rs}    # conformance-suite runner (dev crate)
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`
- Create: `crates/<each-of-11>/Cargo.toml` and `crates/<each-of-11>/src/lib.rs` (empty `// placeholder removed at task N` — only `capyctl-domain` gets real content in Task 2)
- Create: `tests/harness/Cargo.toml`, `tests/harness/src/lib.rs` (empty test harness crate, dev-dependency of others later)

**Interfaces:**
- Produces: workspace named `capyctl`; crates named exactly `capyctl-domain`, `capyctl-store`, `capyctl-scheduler`, `capyctl-protocol`, `capyctl-controller`, `capyctl-router`, `capyctl-agent`, `capyctl-adapters`, `capyctl-launchers`, `capyctl-config`, `capyctl-cli`. All later tasks consume these names.

- [ ] **Step 1: Write workspace root**

```toml
# Cargo.toml
[workspace]
resolver = "2"
members = [
  "crates/capyctl-domain", "crates/capyctl-store", "crates/capyctl-scheduler",
  "crates/capyctl-protocol", "crates/capyctl-controller", "crates/capyctl-router",
  "crates/capyctl-agent", "crates/capyctl-adapters", "crates/capyctl-launchers",
  "crates/capyctl-config", "crates/capyctl-cli", "tests/harness",
]

[workspace.package]
edition = "2021"
version = "0.1.0"
license = "Apache-2.0"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
tonic = "0.13"
prost = "0.13"
tonic-prost = "0.13"
rusqlite = { version = "0.37", features = ["bundled"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
ulid = "1"
sha2 = "0.10"
thiserror = "2"
proptest = "1"
tempfile = "3"
hex = "0.4"
capyctl-domain = { path = "crates/capyctl-domain" }
capyctl-store = { path = "crates/capyctl-store" }
capyctl-scheduler = { path = "crates/capyctl-scheduler" }
capyctl-protocol = { path = "crates/capyctl-protocol" }
capyctl-config = { path = "crates/capyctl-config" }
capyctl-adapters = { path = "crates/capyctl-adapters" }

[profile.dev]
debug = 1
```

```toml
# rust-toolchain.toml
[toolchain]
channel = "stable"
```

```gitignore
/target
*.sqlite3*
```

Each member `Cargo.toml` (example for `capyctl-domain`; others identical minus deps):

```toml
[package]
name = "capyctl-domain"
edition.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
thiserror = { workspace = true }
```

`capyctl-cli` additionally declares `[[bin]] name = "capyctl" path = "src/main.rs"`.

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -w && cargo test -w`
Expected: PASS (all crates empty; zero tests).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore crates tests
git commit -m "feat: scaffold 11-crate workspace with pinned shared deps"
```

---

### Task 2: Lifecycle state machine (`capyctl-domain`)

**Files:**
- Create: `crates/capyctl-domain/src/lib.rs`, `crates/capyctl-domain/src/lifecycle.rs`, `crates/capyctl-domain/src/identity.rs`, `crates/capyctl-domain/src/error.rs`
- Test: same files (`#[cfg(test)]` modules; proptest for transition table)

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 3, 6, 8, 11, 14):

```rust
pub enum LifecycleState { Stopped, Starting, Ready, Draining, Parking, Parked,
                          Waking, Stopping, Reconciling, Failed }
pub enum TransitionError { Illegal { from: LifecycleState, to: LifecycleState } }
impl LifecycleState {
    pub fn can_transition_to(&self, to: LifecycleState) -> bool;
    pub fn is_uncertain(&self) -> bool; // true iff Reconciling
}
pub struct Generation(pub u64);          // monotonic, never reset
pub struct DeploymentId(pub ulid::Ulid);
pub struct OperationId(pub String);
pub struct OwnerAccountId(pub String);
pub struct StaleGenerationError;         // returned when observed < expected
```

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-domain/src/lifecycle.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::LifecycleState::*;

    #[test]
    fn legal_paths_from_spec_6_1() {
        assert!(Stopped.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Ready));
        assert!(Ready.can_transition_to(Draining));
        assert!(Draining.can_transition_to(Parking));
        assert!(Parking.can_transition_to(Parked));
        assert!(Parked.can_transition_to(Waking));
        assert!(Waking.can_transition_to(Ready));
        assert!(Draining.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Stopped));
        assert!(Parked.can_transition_to(Stopping));
        for s in [Stopped, Starting, Ready, Draining, Parking, Parked,
                  Waking, Stopping, Failed] {
            assert!(s.can_transition_to(Reconciling), "{s:?} -> RECONCILING");
        }
        assert!(Reconciling.can_transition_to(Failed));
    }

    #[test]
    fn illegal_paths_rejected() {
        assert!(!Ready.can_transition_to(Parked));     // must drain first
        assert!(!Parked.can_transition_to(Starting));  // must wake
        assert!(!Stopped.can_transition_to(Ready));    // no cold jump
        assert!(!Failed.can_transition_to(Ready));     // recovery first
    }

    #[test]
    fn reconciling_is_the_only_uncertain_state() {
        for s in [Stopped, Starting, Ready, Draining, Parking, Parked,
                  Waking, Stopping, Failed] {
            assert!(!s.is_uncertain());
        }
        assert!(Reconciling.is_uncertain());
    }

    proptest! {
        #[test]
        fn every_legal_transition_pair_is_symmetric_in_table(
            from in prop::sample::select(LifecycleState::ALL.to_vec()),
            to in prop::sample::select(LifecycleState::ALL.to_vec()),
        ) {
            prop_assert_eq!(from.can_transition_to(to),
                            LEGAL.contains(&(from, to)));
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p capyctl-domain`
Expected: FAIL — `LifecycleState`, `can_transition_to` not defined.

- [ ] **Step 3: Implement the transition table**

```rust
// crates/capyctl-domain/src/lifecycle.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    Stopped, Starting, Ready, Draining, Parking, Parked,
    Waking, Stopping, Reconciling, Failed,
}

impl LifecycleState {
    pub const ALL: &'static [LifecycleState] = &[
        Self::Stopped, Self::Starting, Self::Ready, Self::Draining,
        Self::Parking, Self::Parked, Self::Waking, Self::Stopping,
        Self::Reconciling, Self::Failed,
    ];
    pub fn is_uncertain(&self) -> bool { matches!(self, Self::Reconciling) }
    pub fn can_transition_to(&self, to: Self) -> bool {
        LEGAL.contains(&(*self, to))
    }
}

use std::sync::OnceLock;
static TABLE: OnceLock<Vec<(LifecycleState, LifecycleState)>> = OnceLock::new();
pub fn legal_transitions() -> &'static Vec<(LifecycleState, LifecycleState)> {
    TABLE.get_or_init(|| {
        use LifecycleState::*;
        vec![
            (Stopped, Starting), (Starting, Ready),
            (Ready, Draining), (Draining, Parking), (Parking, Parked),
            (Parked, Waking), (Waking, Ready),
            (Draining, Stopping), (Stopping, Stopped),
            (Parked, Stopping),
            (Stopped, Reconciling), (Starting, Reconciling), (Ready, Reconciling),
            (Draining, Reconciling), (Parking, Reconciling), (Parked, Reconciling),
            (Waking, Reconciling), (Stopping, Reconciling), (Failed, Reconciling),
            (Reconciling, Stopped), (Reconciling, Ready), (Reconciling, Failed),
        ]
    }).clone()
}
static LEGAL: OnceLock<Vec<(LifecycleState, LifecycleState)>> = OnceLock::new();
fn table() -> &'static Vec<(LifecycleState, LifecycleState)> {
    LEGAL.get_or_init(legal_transitions)
}
```

(`can_transition_to` reads `table()`; add `#![allow(clippy::...)]` as needed. Export from `lib.rs`: `pub mod lifecycle; pub mod identity; pub mod error; pub use lifecycle::{LifecycleState, legal_transitions};`)

- [ ] **Step 4: Run tests**

Run: `cargo test -p capyctl-domain`
Expected: PASS (including proptest).

- [ ] **Step 5: Identity types + stale-generation rejection (failing test first)**

```rust
// crates/capyctl-domain/src/identity.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_ids_are_sortable_and_stable() {
        let a = DeploymentId::new();
        let b = DeploymentId::new();
        assert!(a.0 < b.0, "ULIDs are monotonic within a millisecond window");
        assert_eq!(a.to_string().len(), 26);
    }

    #[test]
    fn stale_generation_rejected() {
        let g = GenerationMonitor::new();
        let g1 = g.next();               // observe generation 1
        assert!(g.check(g1).is_ok());
        let g3 = g.next(); g.next();     // two more transitions
        assert!(matches!(g.check(g1), Err(StaleGenerationError)));
    }

    #[test]
    fn generation_never_resets() {
        let mut g = GenerationMonitor::new();
        let before = g.peek().0;
        g.next(); g.next();
        assert!(g.peek().0 > before.0);
    }
}
```

Run: `cargo test -p capyctl-domain` → FAIL (types undefined). Implement:

```rust
pub struct DeploymentId(pub ulid::Ulid);
impl DeploymentId {
    pub fn new() -> Self { Self(ulid::Ulid::new()) }
}
pub struct Generation(pub u64);
pub struct OperationId(pub String);
pub struct OwnerAccountId(pub String);

#[derive(Debug, PartialEq)]
pub struct StaleGenerationError;

pub struct GenerationMonitor { current: std::sync::atomic::AtomicU64 }
impl GenerationMonitor {
    pub fn new() -> Self { Self(std::sync::atomic::AtomicU64::new(0)) }
    pub fn advance(&self) -> Generation {
        Generation(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
    }
    pub fn current(&self) -> Generation { Generation(self.0.load(std::sync::atomic::Ordering::SeqCst)) }
    pub fn check(&self, observed: Generation) -> Result<Generation, StaleGenerationError> {
        if observed.0 >= self.current().0 { Ok(observed) }
        else { Err(StaleGenerationError) }
    }
}
```

- [ ] **Step 5b: Run and commit**

Run: `cargo test -p capyctl-domain` → PASS.
```bash
git add crates/capyctl-domain && git commit -m "feat(domain): lifecycle state machine, identities, generation rejection"
```

---

### Task 3: Strict config schema and validation (`capyctl-config`)

**Files:**
- Create: `crates/capyctl-config/src/{lib.rs, strict_yaml.rs, schema.rs, error.rs}`
- Test: `#[cfg(test)]` modules + fixture YAML strings inline

**Interfaces:**
- Consumes: nothing (Task 4's roles wiring uses it).
- Produces:

```rust
pub enum ConfigKind { Server, Host, Deployment, Standalone }
pub struct ConfigError { pub code: ConfigErrorCode, pub path: String, pub detail: String }
pub enum ConfigErrorCode { UnknownField, DuplicateKey, InvalidUnit, MissingRequired,
                           UnsupportedCombination, ConflictingArgs, InvalidCacheRef,
                           ContradictoryConnection, SchemaVersion }
pub fn parse_strict(kind: ConfigKind, text: &str)
    -> Result<serde_json::Value, ConfigError>; // normalized JSON view of validated YAML
pub fn validate(text: &str, expected: ConfigKind) -> Result<(), ConfigError>;
```

- [ ] **Step 1: Failing tests (T03 shapes)**

```rust
// crates/capyctl-config/src/strict_yaml.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_key_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nname: b\n";
        assert!(matches!(validate(y, ConfigKind::Server),
            Err(ConfigError { code: ConfigErrorCode::DuplicateKey, .. })));
    }

    #[test]
    fn unknown_capyctl_field_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nfrobnicate: true\n";
        assert!(matches!(validate(y, ConfigKind::Server),
            Err(ConfigError { code: ConfigErrorCode::UnknownField, .. })));
    }

    #[test]
    fn invalid_unit_rejected() {
        let y = "schema_version: 1\nkind: server\nname: a\nlisteners: {}\n\
                 scheduler:\n  queue:\n    max_buffered_bytes_total: \"64\"\n";
        assert!(matches!(validate(y, ConfigKind::Server),
            Err(ConfigError { code: ConfigErrorCode::InvalidUnit, .. })));
    }

    #[test]
    fn missing_required_rejected() {
        let y = "kind: deployment\nname: d\n"; // schema_version + model missing
        assert!(matches!(validate(y, ConfigKind::Deployment),
            Err(ConfigError { code: ConfigErrorCode::MissingRequired, .. })));
    }

    #[test]
    fn valid_server_parses_to_json_view() {
        let y = "schema_version: 1\nkind: server\nname: lab\n";
        let v = parse_strict(ConfigKind::Server, y).unwrap();
        assert_eq!(v["kind"], "server");
    }
}
```

Run: `cargo test -p capyctl-config` → FAIL.

- [ ] **Step 2: Implement the saphyr-based strict loader**

Mechanics: parse with `saphyr_parser` event stream; track mapping-key sets per node path; any repeat → `DuplicateKey`. Then a hand-written allowlist walk per `ConfigKind` (field sets from SPEC §16): known-field check → `UnknownField`; required-field check → `MissingRequired`; unit parser for `"64MiB"`/`"15m"` style scalars (regex `^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$`) → `InvalidUnit`; `kind` must match `ConfigKind` → `SchemaVersion` mismatch is `InvalidUnit`-class diagnostic. Convert to `serde_json::Value` for the normalized view.

Add to `capyctl-config/Cargo.toml`: `saphyr-parser = "0.13"`, `serde = { workspace = true }`, `serde_json = "1"`, `regex = "1"`, `thiserror = { workspace = true }`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p capyctl-config` → PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/capyctl-config && git commit -m "feat(config): strict YAML parse with duplicate-key and field validation"
```

---

### Task 4: No-config behavior and atomic generation (`capyctl-config`)

**Files:**
- Create: `crates/capyctl-config/src/defaults.rs`
- Test: `#[cfg(test)]` in `defaults.rs` + `crates/capyctl-config/tests/noconfig.rs` (T02, T04)

**Interfaces:**
- Consumes: Task 3 `parse_strict`/`validate`.
- Produces:

```rust
pub enum LoadOutcome { Loaded(String), Generated { config_path: PathBuf, created_identity: bool } }
pub fn resolve_startup(kind: ConfigKind, explicit: Option<&Path>, state_dir: &Path)
    -> Result<LoadOutcome, ConfigError>;
pub fn generate_default(kind: ConfigKind, state_dir: &Path) -> Result<(PathBuf, Vec<u8>), ConfigError>;
```

- [ ] **Step 1: Failing tests (T02, T04)**

```rust
// crates/capyctl-config/tests/noconfig.rs
mod common;
use common::*;

#[test]
fn t02_missing_implicit_generates_once_with_protected_files() {
    let d = temp_state_dir();
    let out = resolve_startup(ConfigKind::Standalone, None, &d).unwrap();
    let LoadOutcome::Generated { config_path, created_identity } = out else { panic!() };
    assert!(created_identity);
    assert!(config_path.exists());
    let mode = config_path.metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "no group/other bits on generated config");
    // Second call loads, not regenerates: mtime unchanged
    let m1 = fs::metadata(&config_path).unwrap().modified().unwrap();
    let out2 = resolve_startup(ConfigKind::Standalone, None, &d).unwrap();
    assert!(matches!(out2, LoadOutcome::Loaded(_)));
    let m2 = fs::metadata(&config_path).unwrap().modified().unwrap();
    assert_eq!(m1, m2);
    assert!(!engine_executed_marker(&d), "no engine execution at startup");
}

#[test]
fn concurrent_starts_do_not_clobber() {
    let d = temp_state_dir();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| resolve_startup(ConfigKind::Standalone, None, &d).unwrap());
        }
    });
    assert_eq!(credential_fingerprint(&d).count(), 1, "credentials created once");
}
```

Run: `cargo test -p capyctl-config` → FAIL.

- [ ] **Step 2: Implement**

`generate_default` renders the standalone shape of SPEC §16.5 (bind `127.0.0.1`, `authentication: admin_token`/`api_key`, relative paths resolved against the config file) with an admin token + API key written to `state_dir/identity/credentials` (0600). Creation: write to `tempfile::NamedTempFile::new_in(parent)`, `fs::rename` (atomic on Linux), `set_permissions(0600)` before rename; state dir created `0700`. Concurrent clobber safety: rename over an existing file is atomic on Linux; the winner is deterministic and the credential is generated once by first-committer — implement via `OpenOptions::new().create_new(true)` on a marker file before rename. Invalid *existing* implicit config → `ConfigError { code: InvalidUnit-class }` (no reset). Explicit path missing → error; explicit path invalid → error. Never execute engines; `resolve_startup` never touches `capyctl-adapters`.

- [ ] **Step 2b: Run tests**

Run: `cargo test -p capyctl-config` → PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/capyctl-config && git commit -m "feat(config): no-config startup matrix with atomic owner-protected generation (T02/T04)"
```

---

### Task 5: Store schema, migrations, transactional acceptance (`capyctl-store`)

**Files:**
- Create: `crates/capyctl-store/src/{lib.rs, schema.rs, migrations.rs, deployments.rs}`
- Test: `#[cfg(test)]` in each; T08/T09 cases in `crates/capyctl-store/tests/acceptance.rs`

**Interfaces:**
- Consumes: `capyctl-domain::{DeploymentId, OperationId, Generation}`.
- Produces:

```rust
pub struct Store { conn: rusqlite::Connection }   // file-backed or :memory: for tests
impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError>;      // applies migrations
    pub fn open_in_memory() -> Result<Store, StoreError>;
    pub fn accept_deployment(&self, req: AcceptDeployment) -> Result<Accepted, StoreError>;
    pub fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, StoreError>;
    pub fn record_operation(&self, op: NewOperation) -> Result<OperationRow, StoreError>;
    pub fn update_operation_state(&self, id: &str, state: OpState, error_code: Option<&str>) -> Result<(), StoreError>;
}
pub struct AcceptDeployment { pub id: DeploymentId, pub name: String, pub kind: String,
    pub route_model_id: Option<String>, pub desired_state: LifecycleState,
    pub schema_version: i64, pub idempotency_key: String,
    pub initial_operation_id: OperationId }
pub enum StoreError { Conflict, IdempotencyConflict, StaleGeneration, Sql(rusqlite::Error), ... }
```

- [ ] **Step 1: Failing tests (T08, T09)**

```rust
// crates/capyctl-store/tests/acceptance.rs
use capyctl_domain::{DeploymentId, OperationId};
use capyctl_store::{AcceptDeployment, Store};

fn req(name: &str, key: &str) -> AcceptDeployment { /* fixed helper: build with fresh ids */ }

#[test]
fn t08_id_returned_after_persistence_survives_new_client() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    let id = {
        let s = Store::open(&path).unwrap();         // "process 1"
        let accepted = s.accept_deployment(req("d1", "k1")).unwrap();
        accepted.deployment_id.clone()
    };                                              // store dropped
    let s2 = Store::open(&path).unwrap();           // "new client"
    let row = s2.get_deployment(&id.to_string()).unwrap().unwrap();
    assert_eq!(row.name, "d1");
    assert!(s2.latest_operation(&id.to_string()).unwrap().is_some());
}

#[test]
fn t09_retry_with_same_key_returns_same_deployment() {
    let s = Store::open_in_memory().unwrap();
    let a = s.accept_deployment(req("d1", "k1")).unwrap();
    let b = s.accept_deployment(req("d1", "k1")).unwrap();  // retry
    assert_eq!(a.deployment_id, b.deployment_id);
    assert_eq!(s.deployment_count().unwrap(), 1);
}

#[test]
fn same_key_different_content_is_conflict() {
    let s = Store::open_in_memory().unwrap();
    s.accept_deployment(req("d1", "k1")).unwrap();
    let mut other = req("d1", "k1");
    other.kind = "host".into();                          // different payload, same key
    assert!(matches!(s.accept_deployment(other), Err(StoreError::IdempotencyConflict)));
}
```

Run: `cargo test -p capyctl-store` → FAIL.

- [ ] **Step 2: Implement schema + acceptance**

`schema.rs` holds the F0 design's DDL verbatim (deployments, operations with `idempotency_key TEXT UNIQUE`, generation_history, owners, reservations, domains, hosts, journal_entries, schema_migrations). `migrations.rs`: `MIGRATIONS: &[&str]` = v1 DDL; `open` wraps `PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;` then applies missing migrations inside one transaction. Acceptance: `conn.execute_batch("BEGIN IMMEDIATE")` → insert deployment → insert operation → commit. On unique violation of `idempotency_key`: read the existing row; if `kind`/`name`/route match the request, return it (idempotent); else `IdempotencyConflict`. Owner-only permissions: after `Store::open` on a file path, `fs::set_permissions(path, 0600)` and `fs::set_permissions(parent, 0700)` (skip for `:memory:`).

- [ ] **Step 2b: Permissions test**

```rust
#[test]
fn store_file_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    Store::open(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(fs::metadata(dir.path()).unwrap().permissions().mode() & 0o077, 0);
}
```

- [ ] **Step 2c: Backup documentation**

Create `crates/capyctl-store/docs/backups.md`: the `sqlite3 .backup`-style consistent-copy procedure, written owner-only (0600) into the same protected state directory as the live store, inheriting the no-secrets/no-inference-bodies content rule; retention/rotation explicitly deferred to a later milestone.

- [ ] **Step 3: Run and commit**

Run: `cargo test -p capyctl-store` → PASS.
```bash
git add crates/capyctl-store && git commit -m "feat(store): transactional acceptance, migrations, derived idempotency, owner-only files"
```

---

### Task 6: Resource ledger — domains, charging, admission (`capyctl-scheduler`)

**Files:**
- Create: `crates/capyctl-scheduler/src/{lib.rs, ledger.rs, admission.rs}`
- Test: `#[cfg(test)]` modules (T26, T27, sub-limit, transition peak, staleness)

**Interfaces:**
- Consumes: `capyctl-domain::OwnerAccountId`.
- Produces:

```rust
pub enum DomainKind { System, DeviceMemory, Filesystem, RemoteStorage }
pub struct Domain { pub id: String, pub kind: DomainKind,
                    pub observed_bytes: Option<i64>, pub observed_at_unix: i64 }
pub struct Reservation { pub owner: OwnerAccountId, pub domain: String,
                         pub bytes: i64, pub phase: Phase,
                         pub category: Option<Category>, pub devices: Vec<String> }
pub enum Phase { Activation, Ready, Parked }
pub enum Category { HostKv, ParkedResidue, SharedService }   // sub-limit tagging
pub struct HostLimits { pub managed_limit: i64, pub free_reserve: i64,
                        pub host_kv_limit: Option<i64>, pub parked_limit: Option<i64>,
                        pub observation_ttl_secs: u64, pub now_unix: i64 }
pub enum BlockReason { InsufficientResources, UnreconciledOwnership, DeviceConflict,
                       CategoryLimit, UnknownTopology, NoSafeEstimate, StaleObservation }
pub fn admit(domains: &[Domain], reservations: &[Reservation],
             candidate: &Candidate, limits: &HostLimits) -> Result<(), BlockReason>;
pub struct Candidate { pub owner: OwnerAccountId, pub domain: String,
    pub activation_peak: i64, pub parked_budget: Option<i64>, // Some => replace-don't-stack
    pub category: Option<Category>, pub devices: Vec<String> }
```

- [ ] **Step 1: Failing tests (T26, T27, sub-limit, peak, stale)**

```rust
// crates/capyctl-scheduler/src/admission.rs
#[cfg(test)]
mod tests {
    use super::*;

    fn sys(observed: i64, age: i64) -> Domain {
        Domain { id: "system".into(), kind: DomainKind::System,
                 observed_bytes: Some(observed), observed_at_unix: age }
    }
    fn res(owner: &str, bytes: i64) -> Reservation {
        Reservation { owner: OwnerAccountId(owner.into()), domain: "system".into(),
                      bytes, phase: Phase::Ready, category: None, devices: vec![] }
    }
    const GiB: i64 = 1024 * 1024 * 1024;
    fn cand_min() -> Candidate {
        Candidate { owner: OwnerAccountId("C".into()), domain: "system".into(),
                    activation_peak: GiB, parked_budget: None,
                    category: None, devices: vec![] }
    }

    #[test]
    fn t26_unified_memory_charged_once() {
        // Spark: one system domain; a CPU-side weight copy is NOT extra capacity.
        let d = sys(128 * GiB, 0);
        let limits = HostLimits { managed_limit: 96 * GiB, free_reserve: 12 * GiB,
                                  host_kv_limit: None, parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 0 };
        let existing = vec![res("A", 48 * GiB)];               // A owns 48 GiB once
        let cand = Candidate { owner: OwnerAccountId("B".into()), domain: "system".into(),
            activation_peak: 48 * GiB, parked_budget: None,
            category: None, devices: vec![] };
        assert!(admit(&[d], &existing, &cand, &limits).is_ok());      // 96 ≤ 96
        let cand2 = Candidate { activation_peak: 49 * GiB, ..cand };
        assert!(matches!(admit(&[d], &existing, &cand2, &limits),
                         Err(BlockReason::InsufficientResources)));   // 97 > 96
    }

    #[test]
    fn t27_disjoint_devices_still_share_system_ram() {
        // Disjoint exclusive device pools do not create extra system-RAM capacity.
        let limits = HostLimits { managed_limit: 56 * GiB, free_reserve: 8 * GiB,
                                  host_kv_limit: None, parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 0 };
        let g0 = Reservation { owner: OwnerAccountId("A".into()), domain: "gpu:0".into(),
                               bytes: 24 * GiB, phase: Phase::Ready,
                               category: None, devices: vec!["gpu:0".into()] };
        let g1 = Reservation { owner: OwnerAccountId("B".into()), domain: "gpu:1".into(),
                               bytes: 24 * GiB, phase: Phase::Ready,
                               category: None, devices: vec!["gpu:1".into()] };
        let cand = Candidate { owner: OwnerAccountId("C".into()), domain: "system".into(),
            activation_peak: 20 * GiB, parked_budget: None,
            category: None, devices: vec!["gpu:2".into()] };
        // Device sets are disjoint; the system domain still enforces its limit.
        assert!(matches!(admit(&[sys(64 * GiB, 0)], &[g0, g1], &cand, &limits),
                         Err(BlockReason::InsufficientResources)));
    }

    #[test]
    fn t24_shape_sublimit_blocks_on_retained_host_kv() {
        // host_kv owners at 9 + 8 > 16 GiB: B is blocked, not shrunk.
        let limits = HostLimits { managed_limit: 96 * GiB, free_reserve: 12 * GiB,
                                  host_kv_limit: Some(16 * GiB), parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 0 };
        let kv_a = Reservation { owner: OwnerAccountId("A-kv".into()),
            domain: "system".into(), bytes: 9 * GiB, phase: Phase::Ready,
            category: Some(Category::HostKv), devices: vec![] };
        let cand = Candidate { owner: OwnerAccountId("B".into()), domain: "system".into(),
            activation_peak: 8 * GiB, parked_budget: None,
            category: Some(Category::HostKv), devices: vec![] };
        assert!(matches!(admit(&[], &[kv_a], &cand, &limits),
                         Err(BlockReason::CategoryLimit)));
    }

    #[test]
    fn replace_dont_stack_includes_candidate_in_charged() {
        // C parked at 2 GiB; activation peak 48 GiB; others hold 96 GiB; limit 96+8 = safe only
        // if the sum includes C's own parked bytes then swaps them out.
        let limits = HostLimits { managed_limit: 100 * GiB, free_reserve: 0,
                                  host_kv_limit: None, parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 0 };
        let others = vec![res("X", 96 * GiB)];
        let c_parked = Reservation { owner: OwnerAccountId("C".into()),
            domain: "system".into(), bytes: 2 * GiB, phase: Phase::Parked,
            category: None, devices: vec![] };
        let cand = Candidate { owner: OwnerAccountId("C".into()), domain: "system".into(),
            activation_peak: 6 * GiB, parked_budget: Some(2 * GiB),
            category: None, devices: vec![] };
        // charged(incl C) = 98; delta = 6-2 = 4; total 102 > 100 → blocked.
        assert!(matches!(admit(&[], &[c_parked].into_iter().chain(others).collect(),
                                &cand, &limits),
                         Err(BlockReason::InsufficientResources)));
    }

    #[test]
    fn stale_observation_blocks_admission() {
        let d = sys(128 * GiB, /*age*/ 3600);
        let limits = HostLimits { managed_limit: 96 * GiB, free_reserve: 12 * GiB,
                                  host_kv_limit: None, parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 3600 };
        assert!(matches!(admit(&[d], &[], &cand_min(), &limits),
                         Err(BlockReason::StaleObservation)));
    }

    #[test]
    fn transition_peak_must_cover_parked_residue() {
        // activation_peak 4 GiB cannot replace a parked 8 GiB residue → reject.
        let limits = HostLimits { managed_limit: 100 * GiB, free_reserve: 0,
                                  host_kv_limit: None, parked_limit: None,
                                  observation_ttl_secs: 60, now_unix: 0 };
        let c_parked = Reservation { owner: OwnerAccountId("C".into()), domain: "system".into(),
            bytes: 8 * GiB, phase: Phase::Parked, category: None, devices: vec![] };
        let cand = Candidate { owner: OwnerAccountId("C".into()), domain: "system".into(),
            activation_peak: 4 * GiB, parked_budget: Some(8 * GiB),
            category: None, devices: vec![] };
        assert!(matches!(admit(&[], &[c_parked], &cand, &limits),
                         Err(BlockReason::InsufficientResources)));
    }
}
```

Run: `cargo test -p capyctl-scheduler` → FAIL.

- [ ] **Step 2: Implement**

`ledger.rs`: per-domain `HashMap<owner, Vec<Reservation>>`. `admission.rs` implements `admit`:

```text
1. candidate domain: find Domain by id; observed_bytes None → UnknownTopology;
   now - observed_at > ttl → StaleObservation.
2. charged(D) = Σ bytes of ALL owners (including candidate's current reservations).
3. transition_delta = candidate.activation_peak, minus parked_budget if the candidate
   itself holds a Parked reservation on D (replace, don't stack).
4. transition-peak validation: activation_peak >= parked residue the group retains
   during wake (v1 rule: activation_peak >= parked_budget); else InsufficientResources
   (the design's §5.2 invariant).
5. pass iff charged + delta ≤ managed_limit AND observed ≥ free_reserve
   AND category sums (owners tagged HostKv / Parked categories) within sub-limits
   AND candidate device set disjoint from other owners' exclusive device sets.
```

Category tagging: `Reservation` gains `category: Option<Category>` (`HostKv`, `ParkedResidue`, `SharedService`); sub-limit checks sum only tagged owners. Shared-service charging: a service owner's bytes are counted once (by owner id) regardless of how many deployments reference it — F0 enforces via unique owner ids.

- [ ] **Step 2b: Run**

Run: `cargo test -p capyctl-scheduler` → PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/capyctl-scheduler && git commit -m "feat(scheduler): ledger with union charging, sub-limits, transition-peak and freshness checks"
```

---

### Task 7: `auto` resolution with provenance (`capyctl-scheduler`)

**Files:**
- Create: `crates/capyctl-scheduler/src/auto.rs`
- Test: `#[cfg(test)]` in `auto.rs`

**Interfaces:**
- Consumes: `DomainKind::System`, constants module.
- Produces:

```rust
pub const AUTO_POLICY_VERSION: u32 = 1;
pub const OBSERVATION_TTL_SECS: u64 = 60;
pub struct ResolvedAuto { pub managed_limit: i64, pub free_reserve: i64,
                          pub policy_version: u32, pub observed_bytes: i64 }
pub fn resolve_auto(observed_system_bytes: i64) -> Result<ResolvedAuto, Diagnostic>;
pub struct Diagnostic { pub code: String, pub detail: String }   // code: "no_safe_estimate"
```

- [ ] **Step 1: Failing tests**

```rust
const GiB: i64 = 1024 * 1024 * 1024;

#[test]
fn constants_match_adr_0005() {
    // 128 GiB observed: managed = min(0.75*128, 128-8) = 96 GiB;
    // reserve = max(8, floor(0.10*128)) = max(8, 12) = 12 GiB (10% floors to whole GiB).
    let r = resolve_auto(128 * GiB).unwrap();
    assert_eq!(r.managed_limit, 96 * GiB);
    assert_eq!(r.free_reserve, 12 * GiB);
    assert_eq!(r.policy_version, AUTO_POLICY_VERSION);
}

#[test]
fn small_host_degenerates_with_diagnostic() {
    // Below 16 GiB observed, managed_limit would fall to <= 8 GiB while
    // free_reserve stays >= 8 GiB — permanently un-admittable, so fail closed
    // with a named diagnostic instead of resolving unusable limits.
    let d = resolve_auto(8 * GiB);
    assert!(matches!(d, Err(Diagnostic { code: "no_safe_estimate", .. })));
}
```

One function, `fn resolve_auto(observed: i64) -> Result<ResolvedAuto, Diagnostic>` — both arms tested. Run → FAIL; implement with i64 GiB arithmetic and floor rounding; degenerate when `observed < 16 GiB` → `no_safe_estimate`. `ResolvedAuto` values are persisted with provenance by the caller (Task 6's ledger records them). Run → PASS. Commit: `feat(scheduler): auto resolution v1 constants with provenance and degenerate diagnostic`.

---

### Task 8: Adapter, launcher, agent traits (`capyctl-adapters`)

**Files:**
- Create: `crates/capyctl-adapters/src/{lib.rs, traits.rs}`
- Test: `#[cfg(test)]` compile-time trait-usage tests (mock impls)

**Interfaces:**
- Produces (used by Tasks 9, 10, 11):

```rust
#[async_trait::async_trait]
pub trait EngineAdapter: Send + Sync {
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError>;
    async fn render_plan(&self, plan: &PlanInput) -> Result<RenderedCommand, AdapterError>;
    async fn check_readiness(&self, member: &MemberRef) -> Result<Readiness, AdapterError>;
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError>;
    async fn park(&self, member: &MemberRef, level: ParkLevel) -> Result<ParkOutcome, AdapterError>;
    async fn restore(&self, member: &MemberRef) -> Result<RestoreOutcome, AdapterError>;
    async fn reload_weights(&self, member: &MemberRef) -> Result<ReloadOutcome, AdapterError>;
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError>;
    async fn cancel_work(&self, member: &MemberRef, req: &RequestRef, require_ack: bool)
        -> Result<CancellationOutcome, AdapterError>;
}
pub enum ParkLevel { One, Two }
pub enum Phase { Startup, Ready, Parking, Restore }   // shared lifecycle phase
pub enum Readiness { Initializing, Ready }
pub trait Launcher: Send + Sync {
    fn spawn(&self, cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError>;
    fn terminate(&self, h: &OwnedHandle, grace: Duration) -> Result<ExitReport, LauncherError>;
    fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus; // detects PID reuse
}
pub struct OwnedHandle { pub pid: u32, pub start_identity: u128 }  // start id = boot-unique
pub enum ParkOutcome { Parked { retained_bytes: i64 }, Uncertain }
pub enum RestoreOutcome { Restored, Uncertain }
pub enum ReloadOutcome { Reloaded, Failed }
pub enum WorkObservation { Idle, Streaming { request_ref: String }, Unknown }
pub enum CancellationOutcome { Acknowledged, Uncertain, Failed }
pub enum AdapterError { Uncertain(String), PolicyDenied, UnsupportedCapability,
                        Crash(Phase), UnsupportedCombination }
```

Note `async_trait` added to workspace deps. Tests: a `NullAdapter` implementing the trait; compile + call `cancel_work` with `require_ack=false` returning `CancellationOutcome::Uncertain` (the contract: no ack → uncertainty, never success). Run → PASS (compile+behavior). Commit: `feat(adapters): engine adapter, launcher, ownership-handle contracts`.

The remaining payload types (`MemberRef`, `EngineState`, `PlanInput`, `RenderedCommand`, `Quiescence`, `RequestRef`, `ExitReport`, `HandleStatus`, `LauncherError`) are plain data structs defined in `traits.rs` within this task; their shape is fixed by the fields Task 9's tests read (`retained_bytes`, `phase`, `pid`, `start_identity`, `valid`).

---

### Task 9: Fake engine harness (`capyctl-adapters::fake`)

**Files:**
- Create: `crates/capyctl-adapters/src/fake/{mod.rs, engine.rs, launcher.rs}`
- Create: `tests/harness/src/lib.rs` (conformance suite entry)
- Test: `crates/capyctl-adapters/tests/fake_scenarios.rs`

**Interfaces:**
- Consumes: Task 8 traits.
- Produces:

```rust
pub struct FakeEngine { /* behavior knobs */ }
impl FakeEngine {
    pub fn new() -> Self;
    pub fn with_startup_delay(self, d: Duration) -> Self;
    pub fn fail_at(self, phase: Phase) -> Self;            // crash injection
    pub fn ambiguous_park(self) -> Self;                   // effect applied, ack lost
    pub fn with_pid_reuse(self) -> Self;
    pub fn with_policy(self, p: ParkPolicy) -> Self;       // deep-park security gate
}
pub enum ParkPolicy { Denied, ExperimentalAllowed }        // default: Denied
// implements EngineAdapter + exposes observation: memory retained per level (L1 keeps
// CPU backup; L2 discards weights+KV, retains buffers), reload_weights counted exactly once.
pub struct FakeLauncher;
impl Launcher for FakeLauncher { /* fake PIDs; with_pid_reuse() respawns same pid, new identity */ }
```

- [ ] **Step 1: Failing scenario tests**

Helpers used below, defined at the top of the test file as trivial fixtures:
`fn member() -> MemberRef` (a `MemberRef { deployment_id, member_id }` pair), `fn req(id: &str) -> RequestRef`, `fn cmd() -> RenderedCommand`, `const BUFFER_RESIDUE: i64` (the fake engine's retained-buffer byte count at level 2).

```rust
// crates/capyctl-adapters/tests/fake_scenarios.rs
#[tokio::test]
async fn slow_startup_liveness_is_not_readiness() {
    let e = FakeEngine::new().with_startup_delay(Duration::from_millis(50));
    let st = e.check_readiness(&member()).await.unwrap();
    assert!(matches!(st, Readiness::Initializing));   // early
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(matches!(e.check_readiness(&member()).await.unwrap(), Readiness::Ready));
}

#[tokio::test]
async fn sleep_level_two_discards_weights_and_kv() {
    let e = FakeEngine::new();
    e.park(&member(), ParkLevel::Two).await.unwrap();
    let obs = e.inspect(&member()).await.unwrap();
    assert_eq!(obs.retained_bytes, BUFFER_RESIDUE);   // weights+KV gone, buffers kept
    assert_eq!(e.reload_weights_count(), 0);          // not yet reloaded
    e.reload_weights(&member()).await.unwrap();
    assert_eq!(e.reload_weights_count(), 1);          // exactly once
}

#[tokio::test]
async fn ambiguous_park_reports_uncertainty_not_success() {
    let e = FakeEngine::new().ambiguous_park();       // effect applied, ack lost
    let out = e.park(&member(), ParkLevel::Two).await;
    assert!(matches!(out, Err(AdapterError::Uncertain(_))));
}

#[tokio::test]
async fn cancellation_without_ack_reports_uncertainty() {
    let e = FakeEngine::new();
    let out = e.cancel_work(&member(), &req("r1"), false).await.unwrap();
    assert!(matches!(out, CancellationOutcome::Uncertain));
}

#[tokio::test]
async fn deep_park_denied_without_policy_opt_in() {
    // Security gate (design §9): level-2 park is experimental, denied by default.
    let e = FakeEngine::new().with_policy(ParkPolicy::Denied);
    let out = e.park(&member(), ParkLevel::Two).await;
    assert!(matches!(out, Err(AdapterError::PolicyDenied)));
    // Opt-in via explicit host policy enables the path:
    let e2 = FakeEngine::new().with_policy(ParkPolicy::ExperimentalAllowed);
    assert!(e2.park(&member(), ParkLevel::Two).await.is_ok());
}

#[tokio::test]
async fn pid_reuse_rejects_stale_handle() {
    let l = FakeLauncher::new().with_pid_reuse();
    let h = l.spawn(&cmd()).unwrap();
    l.terminate(&h, Duration::from_secs(1)).unwrap();
    let h2 = l.spawn(&cmd()).unwrap();                 // same pid, new identity
    assert!(matches!(l.verify_handle(&h), HandleStatus::StaleReused));
    assert!(matches!(l.verify_handle(&h2), HandleStatus::Valid));
}

#[tokio::test]
async fn crash_at_phase_is_reported() {
    let e = FakeEngine::new().fail_at(Phase::Restore);
    assert!(matches!(e.restore(&member()).await, Err(AdapterError::Crash(p))
                     if matches!(p, Phase::Restore)));
}
```

- [ ] **Step 2: Implement the simulator** (state machine over `Phase`, `Mutex` knobs, deterministic virtual clock via `tokio::time`).
- [ ] **Step 3: Run** `cargo test -p capyctl-adapters` → PASS.
- [ ] **Step 4: Commit** `feat(adapters): fake engine with slow start, sleep levels, ambiguous outcomes, pid reuse`.

---

### Task 10: gRPC proto freeze + wire round-trip (`capyctl-protocol`)

**Files:**
- Create: `crates/capyctl-protocol/proto/capyctl/management/v1/management.proto` (Appendix A of the design verbatim — Envelope wired into all commands/reports, fields 1..n frozen), `crates/capyctl-protocol/build.rs`, `crates/capyctl-protocol/src/lib.rs`
- Test: `crates/capyctl-protocol/tests/wire_roundtrip.rs`

**Interfaces:**
- Produces: generated types `capyctl_protocol::pb::*`; `AgentControl` server/client; `PROTOCOL_VERSION = "1"`.

- [ ] **Step 1: Transcribe the proto** from the design's Appendix A exactly (all `Envelope envelope = N;` fields included). `build.rs` uses `tonic-prost-build` (workspace dep) with `protoc` from system or `protobuf-src` — prefer system `protoc`; record in the task output if unavailable.

- [ ] **Step 2: Compile test**

Run: `cargo build -p capyctl-protocol` → PASS.

- [ ] **Step 3: Failing wire round-trip test**

```rust
// crates/capyctl-protocol/tests/wire_roundtrip.rs
#[tokio::test]
async fn agent_control_roundtrip_over_real_channel() {
    // Start an in-process tonic server implementing AgentControl::session that
    // echoes LaunchMember back as ReportOperationResult(state="accepted").
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(server(rx));
    let channel = tonic::transport::Endpoint::from_shared("http://[::1]:0")
        .connect().await.unwrap();   // bind ephemeral; server hands the addr via tx
    let mut client = AgentControlClient::new(channel);
    let (req_tx, req_rx) = mpsc::channel(8);
    req_tx.send(AgentToServer { msg: Some(connect()) }).await.unwrap();
    let mut stream = client.session(tonic::Streaming::from(req_rx)).await.unwrap();
    let resp = stream.message().await.unwrap().unwrap(); // ServerToAgent echo
    assert!(matches!(resp.msg, Some(server_to_agent::Msg::LaunchMember(_))));
    // Envelope present and versioned:
    let lm = match resp.msg { Some(server_to_agent::Msg::LaunchMember(l)) => l, _ => unreachable!() };
    assert_eq!(lm.envelope.expect("envelope wired").protocol_version, "1");
}
```

Run: `cargo test -p capyctl-protocol` → PASS.

- [ ] **Step 4: Version-check + clock-skew tests + commit**

```rust
#[test]
fn protocol_version_is_pinned() { assert_eq!(PROTOCOL_VERSION, "1"); }

#[tokio::test]
async fn deadline_enforced_with_skew_tolerance() {
    // Agent side: a command whose deadline passed 10s ago is accepted (within the
    // 30s tolerance); one that passed 45s ago is expired (beyond tolerance).
    let now = now_unix_ms();
    assert!(deadline_ok(now - 10_000));
    assert!(!deadline_ok(now - 45_000));
}
```
Commit: `feat(protocol): freeze capyctl.management.v1 proto with Envelope wiring, wire round-trip, skew tolerance`.

---

### Task 11: CLI grammar, exit codes, structured output (`capyctl-cli`)

**Files:**
- Create: `crates/capyctl-cli/src/{main.rs, grammar.rs, output.rs, roles.rs}`
- Test: `crates/capyctl-cli/tests/grammar.rs` (T01), `crates/capyctl-cli/tests/errors.rs`

**Interfaces:**
- Consumes: Tasks 3–7 outputs.
- Produces:

```rust
pub enum Command { Start(Role), Init(InitTarget), Invite{..}, Join{..}, List{resource},
    Inspect{resource, id, effective: bool}, Doctor{host}, Qualify{deployment},
    Deploy{file, activate, wait}, Status{deployment, watch},
    Lifecycle{action: LifecycleAction, deployment: String},   // start|park|stop|preinitialize|undeploy
    Validate{file} }
pub fn parse(args: &[OsString]) -> Result<Command, CliError>;  // clap-derive backed
pub struct ExitCode(pub i32);  // mapping fn: error -> exit code per design §7 table
```

- [ ] **Step 1: Failing tests (T01)**

```rust
#[test]
fn action_first_grammar() {
    assert!(matches!(parse(["capyctl", "start", "server"]),
        Ok(Command::Start(Role::Server))));
    assert!(matches!(parse(["capyctl", "deploy", "model"]),
        Ok(Command::Deploy{activate: false, wait: false, ..})));
    assert!(matches!(parse(["capyctl", "stop", "deployment", "dep_x"]),
        Ok(Command::Lifecycle{action: LifecycleAction::Stop, deployment} if deployment == "dep_x")));
    assert!(parse(["capyctl", "server", "run"]).is_err(), "legacy grammar rejected");
    assert!(parse(["capyctl", "start", "power-on", "host-1"]).is_err(),
        "start targets roles, not remote machines");
}

#[test]
fn list_and_status_never_activate() {
    // grammar carries no activation side effect; enforced by Command shape
    // (Status/List carry no boolean activation flag at all).
}
```

Run → FAIL; implement clap derive with subcommand tree `start|init|invite|join|list|inspect|doctor|deploy|status|validate|<lifecycle>`; wire exit-code mapping fn. Run → PASS. Commit: `feat(cli): action-first grammar with stable exit codes and JSON output`.

---

### Task 12: Standalone wiring — full lifecycle exit gate

**Files:**
- Create: `crates/capyctl-controller/src/{operations.rs, lib.rs}` (operation engine: submit → persist → run against fake participants)
- Create: `crates/capyctl-agent/src/lib.rs` (embedded host: supervision loop over fake launcher/adapter)
- Modify: `crates/capyctl-cli/src/roles.rs` (`start standalone` boots embedded server+host)
- Test: `crates/capyctl-cli/tests/standalone_lifecycle.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `Controller::deploy_and_run(req) -> Accepted`; embedded role graph (server + agent in-process, same store).

- [ ] **Step 1: Failing exit-gate test**

```rust
#[tokio::test]
async fn standalone_boot_runs_full_fake_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let app = roles::start_standalone(dir.path()).await.unwrap(); // in-process server+host
    let dep = app.controller.submit_deploy(req_fake_engine("m1")).await.unwrap(); // durable ID
    assert!(app.store.get_deployment(&dep).unwrap().is_some());
    async fn run(app: &App, dep: &str, action: LifecycleAction, want: LifecycleState) {
        let op = app.controller.request_transition(dep, action).await.unwrap();
        app.controller.wait_terminal(&op).await.unwrap();
        let row = app.store.get_deployment(dep).unwrap().unwrap();
        assert_eq!(row.observed_state, want);
    }
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    // park/wake cycle:
    run(&app, &dep, LifecycleAction::Park, LifecycleState::Parked).await;
    run(&app, &dep, LifecycleAction::Start, LifecycleState::Ready).await;
    // stop:
    run(&app, &dep, LifecycleAction::Stop, LifecycleState::Stopped).await;
}
```

Run → FAIL. Implement the minimal operation engine (per design §5: submit transactionally, execute against fake adapter, record evidence in journal_entries, transition observed state strictly through the legal table — RECONCILING on `AdapterError::Uncertain`). Run → PASS.

- [ ] **Step 2: Commit**

```bash
git add crates/capyctl-controller crates/capyctl-agent crates/capyctl-cli
git commit -m "feat(controller): standalone embedded lifecycle over fake engine (F0 exit gate)"
```

---

### Task 13: Remaining spec-id tests — T01–T04, T08, T09, T26, T27 mapping audit

**Files:**
- Create: `tests/mapping/README.md` — table mapping every F0 target test id to its `#[test]` name(s)
- Modify: any gap found (add tests where a target is unclaimed)

**Interfaces:** none new.

- [ ] **Step 1: Write the audit table** (T01 → `capyctl-cli/tests/grammar.rs`, T02 → `capyctl-config/tests/noconfig.rs`, T03 → `strict_yaml` tests, T04 → concurrent-init test, T08/T09 → `capyctl-store/tests/acceptance.rs`, T26/T27 → `capyctl-scheduler` tests; plus fake-engine scenario suite and wire round-trip).
- [ ] **Step 2: Full suite green** — `cargo test -w --workspace` (unit + integration) — and `cargo clippy -w -- -D warnings` clean.
- [ ] **Step 3: Commit** `test(mapping): F0 exit-gate coverage audit — all targeted spec ids claimed`.

---

## Verification workflow (per AGENT_HANDOFF)

Each task reports: implemented requirement slice, commands executed, pass/fail, and fixture-vs-simulator tier. F0 claims nothing about real engines, GPUs, or hardware. The fake-engine suite (Task 9) is the simulator tier; the wire round-trip (Task 10) is simulator-tier transport evidence; no live-engine claims exist in F0.