# Live Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the four problems the TensorFold live run found: Hugging Face link chains are measured, `validate config` without `--host` checks a declared `resources` block, a role can start with no engine and `engine remove` can drop the last profile, and a client hang-up mid-stream cancels the engine request for every engine while the lease stays charged until the engine reports quiescence.

**Architecture:** Four independent slices. (1) The agent's checkpoint walker follows up to 8 store-contained link hops; the controller's checkpoint gate keeps the walker's refusal code in its message. (2) A new `capyctl_config::effective::validate_declared_resources` decodes `resources` with the resolution types and runs the intrinsic checks; `validate config` calls it offline. (3) Standalone start takes its role-level settings from the provider instead of the first installation, so an empty profile list boots; `engine remove` drops the "keep one engine" refusal. (4) The HTTP forwarder stops reading and drops the engine connection when delivery fails and returns `StreamEnded::Cancelled`; the router closes that lease as `LeaseEnd::Cancelling`, which writes a cancellation row beside the still-`inflight` lease (so every existing drain waits for it); a coordinator tick closes cancelling leases once the instance's adapter reports engine-wide quiescence through a new `EngineAdapter::engine_quiescent`.

**Tech Stack:** Rust 2021 workspace (tokio, axum, reqwest, rusqlite, serde_path_to_error), bash live harness under `scripts/live/matrix/`, Astro site synced from `docs/guide/`.

**Spec:** `docs/specs/2026-10-01-live-followups-design.md` (owner decisions 2026-10-01). Read it with this plan. Governing documents: `docs/SPEC.md` §8, §10, §15.3, §20 (T02, T03, T17, T37, T38, T41), §21; ADR 0014 §7; ADR 0018 §4, §5; ADR 0023 §4, §6; `AGENTS.md`.

## Rulings (made while planning; the owner may overturn any)

1. **`cancelling` is a cancellation row, not a new disposition.** The `request_leases.disposition` CHECK allows only `inflight` and `uncertain`, and changing it needs a table rebuild. Instead schema v38 adds `request_lease_cancellations(lease_id, cancelled_at_ms)` with `ON DELETE CASCADE`. A cancelling lease is an `inflight` lease with a cancellation row. Every drain that counts `inflight` leases (switch fairness wait, switch drain, `accept_switch_release`, stop drain) and every park/idle check that tests for any lease therefore waits for it with no query change. Task 10 pins this with tests.
2. **Quiescence is sampled after the hang-up.** `EngineAdapter::engine_quiescent(member, after_ms)` answers `true` only for counters observed at or after `after_ms`. The settler records `asked_at_ms` before the call and closes only cancelling leases whose `cancelled_at_ms <= asked_at_ms`. A sample from before the hang-up can never close a lease.
3. **Quiescence is engine-wide.** Other requests on the same engine keep a cancelling lease charged until they finish too. On a busy engine this is conservative (SPEC §10), and a drain closes admission first, so a switch still converges.
4. **Remote hosts use the host's load report, as W12 does.** `RemoteEngine::engine_quiescent` reuses the W12 `quiescent` predicate (`remote_readiness.rs`): a fresh sample for exactly this launch, taken at or after `after_ms`, with `ingress_in_flight == 0` and `engine_queue() == Some(0)`. For a TensorFold scope the host agent also reads `/health` and reports at least one running request unless `/health` reads idle (Task 13). That way the remote evidence includes `requests_running: 0` and `busy: false`, as the design asks, without a new protocol message.
5. **A slow client is a hung-up client.** `ResponseSink::send` already fails after 10 s without delivery. That failure now cancels upstream too, which matches what the client sees (nothing more is delivered either way).
6. **Non-streaming requests are unchanged.** The design covers streams only. A non-streaming request whose client leaves keeps today's behaviour.
7. **Offline TensorFold detection.** The engine family is known only from the host profile. Offline, a deployment counts as TensorFold when its `runtime_profile` (or `engine`) is `tensorfold` or it has an `engine_config.tensorfold` block. A TensorFold profile under another name is caught only with `--host`, and the text output says so.
8. **Offline device claims.** The resolution rule is applied as is: phase claims must match the deployment's top-level `devices`, an absent list being empty. This is exactly what the server does (`effective.rs` `d.devices.take().unwrap_or_default()`).
9. **An empty role refuses new deploys as today.** "Accepts and keeps deployments" means existing deployments stay stored and the role keeps answering. A new `deploy model` still fails fast with `no_runtime_profiles` or `profile_not_published` (ADR 0018 §7). `start standalone --model` with no engine is refused with a message that names `capyctl engine add`.
10. **No new ADR.** ADR 0014 gains Amendment A5 (link chains) and ADR 0018 gains Amendment A2 (empty role). SPEC §8, §10, T02 and T17 are amended in place, and the §21 "Later amendments" entry cites the design note. SPEC §8 never stated "no engine, no boot"; only code comments did, and those comments are removed.
11. **Hop count.** A link to a file is 1 hop. A chain of 8 links ending at a regular file is accepted; a chain of 9 is refused `unsafe_file`. Loops are refused by the same bound.

## Global Constraints

- Link chains: at most 8 hops; each hop resolved lexically against the directory that holds the current link, opened from the store descriptor with `O_NOFOLLOW`, inside the model store; the walk ends at a regular file; a directory, an escape, a loop or a ninth hop is `unsafe_file` (design §1).
- The manifest records the first link's path and the final file's identity; plain files and one-hop links keep today's digests (design §1).
- Gate wording: a checkpoint that cannot be measured reports `the checkpoint could not be measured (<reason>)`; only a real mismatch says `does not match its recorded digest` (design §1).
- Offline validation decodes `resources` with the server's own types and refuses a TensorFold deployment without `resources` (design §2, ADR 0023 §4).
- A role with no engine starts, publishes an empty profile list, places nothing, and its banner and `capyctl status` name `capyctl engine add` (design §3).
- ADR 0018 §4 is unchanged: a published profile is not removed while the role is unreachable (design §3).
- On hang-up the forwarder stops reading and closes the engine connection; the lease stays charged until engine-wide quiescence (no running and no waiting requests). vLLM gauges are `vllm:num_requests_running` and `vllm:num_requests_waiting` (vLLM 0.29, `vllm/v1/metrics/loggers.py`, as pinned in `crates/capyctl-agent/src/load.rs`). SGLang gauges are `sglang:num_running_reqs` and `sglang:num_queue_reqs` (SGLang 0.5.20, `crates/capyctl-adapters/src/sglang/observation.rs`). TensorFold is idle when `/health` reads `requests_running: 0` and `busy: false` (`HealthReport::idle`) (design §4).
- No partial stream is replayed (T38); streams whose client stays connected are unchanged (design §4).
- Cite the governing requirement inline (`// SPEC §10 (amended 2026-10-01): ...`, `// ADR 0014 §7 (A5): ...`). Tag every new test with its acceptance ID: T37 for checkpoint rows (the existing walker tests use `// T37, ADR 0014 §7`), T03 for `validate config`, T02 (plus T16 and T32 for `engine remove`) for the empty role, T17 and T38 for the hang-up, plus T41 on TensorFold-specific tests.
- Minimal comments. Remove comments that the change makes false (listed in each task).
- Verification before every commit: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; the core suite `cargo test -p capyctl-adapters -p capyctl-store -p capyctl-controller -p capyctl-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; `cargo test --workspace --all-targets --locked`. Tasks touching `docs/guide/` also run `cd site && npm run check`.
- CPU and Fake-engine tests are not qualification. Only Task 17's live rows qualify these changes. Say so in every status claim.
- Commit messages and documents are normal English. No AI attribution or co-author trailers, no machine names, IPs or maintainer home paths. Use "host A" and "host B".

## Review Focus

These are inputs the spec implies but no main-path test exercises. They are the ones most likely to bite a user first, and each has a pinning test in the task named.

1. **A chain whose second hop is relative to its own directory, not the snapshot's.** This is the real `huggingface_hub` layout: `blobs/<h> -> ../../blobs/<xx>/<sha>`. A resolver that joins every hop to the snapshot directory measures the wrong file or refuses a good cache. Test: Task 1, `a_relative_second_hop_resolves_against_its_own_directory`.
2. **Another request still streaming on the engine when one client hangs up.** The cancelling lease must stay charged until that request finishes as well, then close. Test: Task 12, `a_cancelling_lease_waits_for_the_other_requests_on_the_engine`.
3. **A server restart while a lease is cancelling.** The session-start sweep makes the lease `uncertain`. The settler must never close an `uncertain` lease because of its old cancellation row. Test: Task 10, `a_cancelling_lease_is_uncertain_after_a_restart`.
4. **A quiescence sample older than the hang-up.** The engine read idle a moment before the client left. That sample must not close the lease. Test: Task 12, `a_quiescence_sample_before_the_hang_up_closes_nothing`.
5. **`engine add` on a running standalone after the last profile was removed.** The new profile is published and placement reopens without a restart. Test: Task 6, `the_last_profile_is_removed_and_a_new_one_is_published_live`.

## Parallel groups and file ownership

Each group can run in its own worktree from the branch head. Within a group, tasks run in the order listed. No file is edited by two groups, with one exception: `crates/capyctl-controller/src/checkpoint_digests.rs` is edited by Task 2 (gate, lines ~600-705) and by Task 9 (the `impl EngineAdapter for CheckpointGate` block, ~706-770). Rebase Task 9 onto Task 2 before merging it.

| Group | Tasks | Files owned |
|---|---|---|
| 1 link chains | 1, 2 | `crates/capyctl-agent/src/checkpoint.rs`, `checkpoint/tests.rs`; `crates/capyctl-controller/src/checkpoint_digests.rs` (gate part), `checkpoint_digests/tests.rs` |
| 2 offline validation | 3 → 4 | `crates/capyctl-config/src/effective.rs` (one `mod` line), `effective/declared_resources.rs` (new), `crates/capyctl-config/tests/declared_resources.rs` (new); `crates/capyctl-cli/src/validate.rs`, `views.rs`, `crates/capyctl-cli/tests/validate_config.rs` |
| 3 empty role | 5 → 6, 5 → 7 | `crates/capyctl-cli/src/roles.rs`, `role_text.rs`, `main.rs`, `client.rs`, `standalone_engines.rs`, `engine.rs`; `crates/capyctl-cli/tests/standalone_start.rs`, `standalone_engines.rs`; `scripts/live/matrix/rows/M75.sh`, `scripts/live/matrix/README.md` |
| 4 hang-up cancel | 8 → 9; 10 (parallel to 8); 11 after 8 and 10; 12 after 9 and 10; 13 (any time); 14 after 11 and 12 | `crates/capyctl-adapters/src/traits.rs`, `forward.rs`, `vllm/adapter.rs`, `sglang/adapter.rs`, `tensorfold/adapter.rs`, `tests/engine_contract.rs`; `crates/capyctl-testkit/src/fake_engine.rs`; `crates/capyctl-store/src/schema.rs`, `migrations.rs`, `dispatch.rs`, `request_lease_cancellations.rs` (new), `lib.rs`; `crates/capyctl-controller/src/request_leases.rs`, `installation_gate.rs`, `checkpoint_digests.rs` (adapter impl only), `remote_execution.rs`, `remote_readiness.rs`, `load_table.rs`, `coordinator/worker.rs`, `coordinator/tests_cancellation.rs` (new); `crates/capyctl-router/src/stream.rs`, `tests/stream_safety.rs`, `tests/router_stream.rs`; `crates/capyctl-agent/src/load.rs`, `crates/capyctl-agent/tests/load.rs`; `crates/capyctl-cli/tests/role_shutdown.rs` |
| 5 documents | 15, 16 (parallel to everything) | `docs/SPEC.md`, `docs/design/adr/0014-*.md`, `docs/design/adr/0018-*.md`; `docs/guide/*.md`, `docs/operations/configuration.md`, `docs/operations/release-notes-0.1.1.md`, `site/src/content/docs/docs/**` (generated by `npm run sync`) |
| live | 17 (after every group merges) | `docs/runbooks/f2-current-status.md` |

---

## Group 1: Hugging Face cache link chains

### Task 1: The walker follows a bounded, store-contained link chain

**Files:**
- Modify: `crates/capyctl-agent/src/checkpoint.rs` (the `libc::S_IFLNK` arm in `Walker::directory`, ~785-794; a new constant beside `MAX_DIRECTORIES`)
- Test: `crates/capyctl-agent/src/checkpoint/tests.rs` (new tests; edit case 4 of `links_that_escape_the_store_or_name_directories_are_refused`, ~350-354)

**Interfaces:**
- Consumes: `Walker::link_target(&mut self, resolved: &Path) -> Result<(Arc<OwnedFd>, CString), CheckpointError>`, `read_link_at`, `lstat_at`, `normalize`, `os_error`, `Walker::push`.
- Produces: `const MAX_LINK_HOPS: usize = 8;`. No public signature changes. `CheckpointVerifier::measure` and `verify` accept chains.

- [ ] **Step 1: Write the failing tests.** Add to `checkpoint/tests.rs` (helpers `Store`, `FILES`, `sha`, `symlink` are already there):

```rust
/// The real `huggingface_hub` shared-blob layout: a snapshot file links to a
/// per-model blob that links again into the hub-wide blob store.
fn chained_cache(store: &Store) -> PathBuf {
    let hub = store.root.join("hub");
    let snapshot = hub.join("models--toy/snapshots/abc");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::create_dir_all(hub.join("models--toy/blobs")).unwrap();
    for (name, bytes) in FILES {
        let digest = sha(bytes);
        let shared = hub.join("blobs").join(&digest[..2]);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join(&digest), bytes).unwrap();
        symlink(
            format!("../../blobs/{}/{digest}", &digest[..2]),
            hub.join("models--toy/blobs").join(&digest),
        )
        .unwrap();
        let link = snapshot.join(name);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let up = "../".repeat(name.matches('/').count());
        symlink(format!("{up}../../blobs/{digest}"), &link).unwrap();
    }
    snapshot
}

// T37, ADR 0014 §7 (A5): a two-hop chain inside the store measures to the
// digest of the same bytes stored plainly.
#[test]
fn a_two_hop_hugging_face_chain_measures_like_a_plain_copy() {
    let store = Store::new();
    let snapshot = chained_cache(&store);
    let plain = Store::new();
    let copy = plain.checkpoint("toy", FILES);
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(
        verifier.measure(&store.root, &snapshot).unwrap().manifest.digest,
        verifier.measure(&plain.root, &copy).unwrap().manifest.digest,
    );
}

// T37, ADR 0014 §7 (A5): the second hop is resolved against the directory
// holding the first link's target, not against the snapshot.
#[test]
fn a_relative_second_hop_resolves_against_its_own_directory() {
    let store = Store::new();
    let snapshot = chained_cache(&store);
    // A decoy where a snapshot-relative resolver would land.
    let decoy = snapshot.join("../../blobs");
    for (_, bytes) in FILES {
        let digest = sha(bytes);
        std::fs::create_dir_all(decoy.join(&digest[..2])).unwrap();
        std::fs::write(decoy.join(&digest[..2]).join(&digest), b"decoy").unwrap();
    }
    let plain = Store::new();
    let copy = plain.checkpoint("toy", FILES);
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(
        verifier.measure(&store.root, &snapshot).unwrap().manifest.digest,
        verifier.measure(&plain.root, &copy).unwrap().manifest.digest,
    );
}

fn chain(checkpoint: &Path, links: usize) {
    std::fs::write(checkpoint.join("hop0.json"), "{}").unwrap();
    for hop in 1..=links {
        symlink(format!("hop{}.json", hop - 1), checkpoint.join(format!("hop{hop}.json"))).unwrap();
    }
}

// T37, ADR 0014 §7 (A5): eight hops are followed; a ninth is refused.
#[test]
fn eight_hops_are_followed_and_a_ninth_is_refused() {
    for (links, ok) in [(8, true), (9, false)] {
        let store = Store::new();
        let checkpoint = store.checkpoint("toy", FILES);
        chain(&checkpoint, links);
        let result = CheckpointVerifier::in_memory().measure(&store.root, &checkpoint);
        if ok {
            assert!(result.is_ok(), "{links} links: {result:?}");
        } else {
            assert!(matches!(result, Err(CheckpointError::UnsafeFile)), "{links} links: {result:?}");
        }
    }
}

// T37, ADR 0014 §7 (A5): a loop, a chain that leaves the store and a chain
// that ends at a directory are refused.
#[test]
fn chains_that_loop_escape_or_end_at_a_directory_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "x").unwrap();
    type Case<'a> = &'a dyn Fn(&Path, &Path);
    let cases: &[Case] = &[
        &|c, _| {
            symlink("b.json", c.join("a.json")).unwrap();
            symlink("a.json", c.join("b.json")).unwrap();
        },
        &|c, _| {
            symlink(outside.path().join("secret"), c.join("out1")).unwrap();
            symlink("out1", c.join("out2")).unwrap();
        },
        &|c, s| {
            std::fs::create_dir_all(s.join("other")).unwrap();
            symlink(s.join("other"), c.join("d1")).unwrap();
            symlink("d1", c.join("d2")).unwrap();
        },
    ];
    for (index, case) in cases.iter().enumerate() {
        let store = Store::new();
        let checkpoint = store.checkpoint("toy", FILES);
        case(&checkpoint, &store.root);
        let error = CheckpointVerifier::in_memory()
            .measure(&store.root, &checkpoint)
            .unwrap_err();
        assert!(matches!(error, CheckpointError::UnsafeFile), "case {index}: {error:?}");
    }
}
```

In `links_that_escape_the_store_or_name_directories_are_refused`, delete case 4 (hop2 → hop1 → real.json, ~350-354), because a chain inside the store is now accepted. Change the header comment to `// T37, ADR 0014 §7: escapes are refused, never followed (chains: A5).`

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-agent --lib checkpoint::tests -- --test-threads=4`
Expected: FAIL. The two chain tests and the eight-hop case fail with `UnsafeFile`.

- [ ] **Step 3: Implement.** Add `/// ADR 0014 §7 (A5): the longest link chain a checkpoint file may use.` and `const MAX_LINK_HOPS: usize = 8;` beside `MAX_DIRECTORIES`. Replace the `S_IFLNK` arm with:

```rust
libc::S_IFLNK => {
    // ADR 0014 §7 (A5): each hop resolves against the directory holding the
    // link and must stay inside the store; the chain ends at a regular file.
    let mut holder = canonical.to_path_buf();
    let mut target = read_link_at(fd.as_raw_fd(), &leaf)?;
    let mut hops = 1;
    loop {
        let resolved = normalize(&holder.join(Path::new(std::ffi::OsStr::from_bytes(&target))));
        let (parent, name) = self.link_target(&resolved)?;
        let st = lstat_at(parent.as_raw_fd(), &name).map_err(os_error)?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                self.push(path, parent, name, FileIdentity::of(&st))?;
                break;
            }
            libc::S_IFLNK if hops < MAX_LINK_HOPS => {
                target = read_link_at(parent.as_raw_fd(), &name)?;
                holder = resolved.parent().ok_or(CheckpointError::UnsafeFile)?.to_path_buf();
                hops += 1;
            }
            _ => return Err(CheckpointError::UnsafeFile),
        }
    }
}
```

`canonical` is the walk's canonical directory path; adjust the borrow if its type is `&Path`. `link_target` already opens every directory on the way from `store_fd` with `O_NOFOLLOW` and refuses anything outside the store.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-agent --lib checkpoint -- --test-threads=4`
Expected: PASS, including the existing `links_inside_the_store_are_followed_as_the_hugging_face_layout`, whose one-hop digest is unchanged.

- [ ] **Step 5: Run the verification commands** (Global Constraints) and commit.

```bash
git add crates/capyctl-agent/src/checkpoint.rs crates/capyctl-agent/src/checkpoint/tests.rs
git commit -m "fix: follow Hugging Face link chains inside the model store"
```

### Task 2: The checkpoint gate keeps the refusal reason

**Files:**
- Modify: `crates/capyctl-controller/src/checkpoint_digests.rs`: `enum GateRefusal` (~698), `CheckpointGate::check` (~658), `verified` (~638), `verified_wake` (~648), and `first_placement_digest` (~148), whose `unrecorded` text has the same collapse.
- Test: `crates/capyctl-controller/src/checkpoint_digests/tests.rs`

**Interfaces:**
- Consumes: `measure_locally(...) -> Result<Verification, MeasureError>` with `MeasureError::Refused(String)` carrying `CheckpointError::code()`.
- Produces: `enum GateRefusal { Mismatch, Unavailable(String) }`. The texts are `the checkpoint could not be measured (<reason>)` and `the checkpoint does not match its recorded digest`.

- [ ] **Step 1: Write the failing test** in `checkpoint_digests/tests.rs`, modelled on `the_embedded_gate_verifies_before_initialize_and_restore`:

```rust
// T37, ADR 0014 §7: a checkpoint that cannot be measured says why; only a
// measured difference is a digest mismatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmeasurable_checkpoint_names_its_reason_not_a_mismatch() {
    let f = fixture(|_| {});
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("config.json"), "{}").unwrap();
    let config = f.models.path().join("toy/config.json");
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(outside.path().join("config.json"), &config).unwrap();
    let work = {
        let o = f.owner.lock().unwrap();
        o.store().accept_start(o.session(), &f.fence, 100, 100_100).unwrap();
        o.store().next_initialize(o.session()).unwrap().unwrap()
    };
    let inner = Arc::new(Counting::default());
    let gate = CheckpointGate::new(
        inner.clone(),
        f.owner.clone(),
        Arc::new(CheckpointVerifier::in_memory()),
        &work,
    );
    let refused = gate
        .execute_persisted(&step(&f, RuntimeAction::Initialize))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, RuntimeError::Uncertain(text)
            if text == "the checkpoint could not be measured (unsafe_file)"),
        "{refused:?}"
    );
    assert_eq!(inner.0.load(Ordering::SeqCst), 0, "nothing reached the engine");
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p capyctl-controller --lib checkpoint_digests::tests::an_unmeasurable -- --test-threads=4`
Expected: FAIL. The text is `the checkpoint does not match its recorded digest`.

- [ ] **Step 3: Implement.** Make `GateRefusal::Unavailable` carry a `String`. In `check`, map `measure_locally` errors as `MeasureError::Refused(code) => Unavailable(code)` and `MeasureError::Unavailable => Unavailable("unavailable".into())`. Map store and record failures to `Unavailable("not_recorded".into())`. `verified` maps `Mismatch` to the existing text and `Unavailable(reason)` to `format!("the checkpoint could not be measured ({reason})")`. `verified_wake` keeps its mapping (`Unavailable(_) => RuntimeError::Unsupported`). In `first_placement_digest`, a `MeasureError::Refused(code)` from `measure()` becomes `RuntimeError::Uncertain(format!("the checkpoint could not be measured ({code})"))`; the other failures keep `the checkpoint digest was not recorded before launch`. Update the doc comment on `verified` to `ADR 0014 §7: before Initialize a failure to verify is uncertain; the message keeps the walker's reason.`

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-controller --lib checkpoint_digests -- --test-threads=4`
Expected: PASS. `the_embedded_gate_verifies_before_initialize_and_restore` still sees `recorded digest` (its file content changed, so the measurement differs).

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-controller/src/checkpoint_digests.rs crates/capyctl-controller/src/checkpoint_digests/tests.rs
git commit -m "fix: say why a checkpoint could not be measured instead of reporting a mismatch"
```

---

## Group 2: `validate config` without `--host`

### Task 3: `validate_declared_resources`

**Files:**
- Create: `crates/capyctl-config/src/effective/declared_resources.rs`
- Modify: `crates/capyctl-config/src/effective.rs` (add `mod declared_resources;` and `pub use declared_resources::validate_declared_resources;` beside the other `effective/` submodules)
- Test: `crates/capyctl-config/tests/declared_resources.rs`

**Interfaces:**
- Consumes (private to `effective`, visible to a child module): `RawRecipe`, `decode::<T>(value, path)`, `raw_recipe(RawRecipe) -> Result<RecipeFootprints, ConfigError>`, `core::validate_resources_intrinsic(&RecipeFootprints, &[DeviceClaim])`, `DeviceClaim`, `ConfigError::new(ConfigErrorCode::MissingRequired, path, text)`.
- Produces: `pub fn validate_declared_resources(deployment: &serde_json::Value) -> Result<(), crate::ConfigError>`.

- [ ] **Step 1: Write the failing tests** in `crates/capyctl-config/tests/declared_resources.rs`:

```rust
//! SPEC §15.3, ADR 0023 §4: `resources` is checked offline with the server's
//! own decoding.
use capyctl_config::effective::validate_declared_resources;
use serde_json::{json, Value};

fn phase(bytes: &str, devices: Value) -> Value {
    json!({"allocations": [{"domain": "unified", "bytes": bytes, "host_kv_bytes": "0B"}], "devices": devices})
}

fn deployment(engine: &str, resources: Option<Value>) -> Value {
    let gpu = json!([{"id": "gpu0", "sharing": "exclusive"}]);
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "m",
        "runtime_profile": engine, "devices": gpu});
    if let Some(r) = resources {
        d["resources"] = r;
    }
    d
}

fn recipe() -> Value {
    let gpu = json!([{"id": "gpu0", "sharing": "exclusive"}]);
    json!({"cold": phase("32GiB", gpu.clone()), "ready": phase("30GiB", gpu.clone()),
        "parking": phase("30GiB", gpu.clone()), "parked": phase("0B", json!([])),
        "wake": phase("32GiB", gpu)})
}

// T03 T41: the guide's TensorFold block passes offline.
#[test]
fn a_complete_resources_block_passes() {
    validate_declared_resources(&deployment("tensorfold", Some(recipe()))).unwrap();
}

// T03: the live finding, a block without `host_kv_bytes` and `devices`.
#[test]
fn a_block_missing_required_fields_is_refused_with_its_path() {
    let mut r = recipe();
    r["cold"] = json!({"allocations": [{"domain": "unified", "bytes": "32GiB"}]});
    let error = validate_declared_resources(&deployment("vllm", Some(r))).unwrap_err();
    assert!(error.path.starts_with("resources.cold"), "{error:?}");
}

// T03: phase claims must match the deployment's devices, as at resolution.
#[test]
fn a_phase_claim_that_is_not_the_deployments_is_refused() {
    let mut d = deployment("vllm", Some(recipe()));
    d["devices"] = json!([{"id": "gpu1", "sharing": "exclusive"}]);
    let error = validate_declared_resources(&d).unwrap_err();
    assert_eq!(error.path, "resources.devices");
}

// T03 T41, ADR 0023 §4: a TensorFold deployment states resources, offline too.
#[test]
fn a_tensorfold_deployment_without_resources_is_refused() {
    for d in [
        deployment("tensorfold", None),
        {
            let mut d = deployment("tf-local", None);
            d["engine_config"] = json!({"tensorfold": {"thinking": false}});
            d
        },
    ] {
        let error = validate_declared_resources(&d).unwrap_err();
        assert_eq!(error.path, "resources", "{d}");
    }
    validate_declared_resources(&deployment("vllm", None)).unwrap();
}
```

Check the `ConfigError` field names (`path`) against `crates/capyctl-config/tests/tensorfold.rs:148`, which asserts `error.path == "resources"`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-config --test declared_resources`
Expected: FAIL to compile (`validate_declared_resources` not found).

- [ ] **Step 3: Implement** `declared_resources.rs`:

```rust
//! SPEC §15.3 (2026-10-01 follow-up): the `resources` checks that need no host.
use super::{core, decode, raw_recipe, DeviceClaim, RawRecipe};
use crate::{ConfigError, ConfigErrorCode};
use serde_json::Value;

/// Decode a declared `resources` block with the types resolution uses and run
/// the intrinsic recipe and device-claim checks against the declared
/// `devices`. A TensorFold deployment must declare one (ADR 0023 §4); offline
/// the family is known from `runtime_profile: tensorfold` or an
/// `engine_config.tensorfold` block.
pub fn validate_declared_resources(deployment: &Value) -> Result<(), ConfigError> {
    let Some(resources) = deployment.get("resources") else {
        let tensorfold = ["runtime_profile", "engine"]
            .iter()
            .any(|key| deployment[*key].as_str() == Some("tensorfold"))
            || deployment["engine_config"].get("tensorfold").is_some();
        if tensorfold {
            return Err(ConfigError::new(
                ConfigErrorCode::MissingRequired,
                "resources",
                "a TensorFold deployment states resources: TensorFold sizes itself from free \
                 memory and has no flag that caps it",
            ));
        }
        return Ok(());
    };
    let recipe = raw_recipe(decode::<RawRecipe>(resources, "resources")?)?;
    let devices: Vec<DeviceClaim> = match deployment.get("devices") {
        Some(value) => decode(value, "devices")?,
        None => Vec::new(),
    };
    core::validate_resources_intrinsic(&recipe, &devices)
}
```

If `decode` prefixes paths differently from `resources.cold...`, keep its behaviour and adjust the test's `starts_with` to the prefix resolution reports for the same input. The point is that offline and server errors match. Move the TensorFold message into one `const` shared with `resolve_effective_with_checkpoint` (`effective.rs` ~1063), so the two texts cannot drift.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-config --test declared_resources && cargo test -p capyctl-config --test tensorfold`
Expected: PASS.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-config/src/effective.rs crates/capyctl-config/src/effective/declared_resources.rs crates/capyctl-config/tests/declared_resources.rs
git commit -m "feat: check a declared resources block without a host"
```

### Task 4: `validate config` runs it and says what still needs a host

**Files:**
- Modify: `crates/capyctl-cli/src/validate.rs` (the Deployment arm, ~139-173)
- Modify: `crates/capyctl-cli/src/views.rs` (`fn validate`, ~218)
- Test: `crates/capyctl-cli/tests/validate_config.rs`

**Interfaces:**
- Consumes: `capyctl_config::effective::validate_declared_resources(&Value)` (Task 3).
- Produces: the JSON shape is unchanged (`requires_server` list). The text output gains a `Not checked` section listing `requires_server` when `resolved_against` is null.

- [ ] **Step 1: Write the failing tests** in `validate_config.rs`, using the file's `write(root, name, text)` and `validate(args) -> (i32, Value, String)` helpers:

```rust
// T03, SPEC §15.3: a resources block the server would refuse fails offline.
#[test]
fn an_incomplete_resources_block_fails_without_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(root.path(), "d.yaml", "name: m\nengine: vllm\nmodel: toy\nresources:\n  cold:\n    allocations: [{domain: unified, bytes: 32GiB}]\n");
    let (code, out, _) = validate(&[file.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.to_string().contains("resources.cold"), "{out}");
}

// T03 T41, ADR 0023 §4: a TensorFold deployment without resources fails offline.
#[test]
fn a_tensorfold_deployment_without_resources_fails_without_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(root.path(), "d.yaml", "name: m\nengine: tensorfold\nmodel: toy\nengine_config: {context_length: 32768}\n");
    let (code, out, _) = validate(&[file.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.to_string().contains("resources"), "{out}");
}

// T03, SPEC §15.3: the text output names what was not checked.
#[test]
fn the_text_output_names_what_still_needs_a_host() {
    let root = tempfile::tempdir().unwrap();
    let file = write(root.path(), "d.yaml", "name: m\nengine: vllm\nmodel: toy\n");
    let out = support::capyctl()
        .args(["validate", "config", file.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Not checked"), "{text}");
    assert!(text.contains("--host"), "{text}");
}
```

Check the exit code for a structurally invalid document (`a_deployment_without_a_host_is_checked_structurally_only` expects 2) and use the same value.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-cli --test validate_config -- --test-threads=4`
Expected: FAIL. The first two exit 0; the text has no `Not checked`.

- [ ] **Step 3: Implement.** In `validate.rs`, after `validate_declared_startup`, add:

```rust
// SPEC §15.3, ADR 0023 §4: `resources` is decoded as the server decodes it.
capyctl_config::effective::validate_declared_resources(&deployment)
    .map_err(|e| named(file, Some(kind), &e))?;
```

Change the first `requires_server` line to `resolution against a host: pass --host <host.yaml> to check the runtime profile, placement, the host's devices and capacity, and timeouts, and to see the host's defaults; a TensorFold profile under another name than tensorfold is recognised only there`. In `views.rs` `fn validate`, when `resolved_against` is null and `requires_server` is a non-empty array, append a `Not checked` heading followed by one indented line per item. Follow the file's existing row helpers (ADR 0021 text style).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-cli --test validate_config -- --test-threads=4`
Expected: PASS, including `every_documented_example_passes_validate_config` (if a documented example now fails, the example is wrong: fix the example in `docs/`, not the check).

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-cli/src/validate.rs crates/capyctl-cli/src/views.rs crates/capyctl-cli/tests/validate_config.rs
git commit -m "fix: validate config checks resources offline and lists what needs a host"
```

---

## Group 3: Removing the last engine profile

### Task 5: A role starts with no engine profile

**Files:**
- Modify: `crates/capyctl-cli/src/roles.rs`: `EngineProvider` trait; `EnvEngineProvider::installations` (~1020-1063, drop the empty refusal); `role_installation_as` (~895, extract the role-level part); `start_standalone_in` (~1740-1755, stop taking role settings from `named.first()`); the default-deploy path (~585, message); `StartError::NoEngineInstallation` doc (~671, remove "Spec §8: no engine installation, no boot").
- Modify: `crates/capyctl-cli/src/role_text.rs` (`banner_text`, ~94) and `crates/capyctl-cli/src/main.rs` (~491, the banner value) to show an empty engine list.
- Modify: `crates/capyctl-cli/src/client.rs` (`capyctl status`, ~1119) to name `capyctl engine add` when the installation view is null.
- Modify: `crates/capyctl-cli/src/standalone_engines.rs` only as far as `EmbeddedHost::new` and `resolve` must accept an empty list (no refusal text changes here; Task 6 owns removal).
- Test: `crates/capyctl-cli/tests/standalone_start.rs` (replace `standalone_refuses_to_boot_without_an_engine_installation`, ~42-75), `crates/capyctl-cli/src/role_text.rs` unit tests.

**Interfaces:**
- Produces: `pub struct RoleSettings { pub runtime_dir: PathBuf, pub engine_ports: PortRange, pub models_root: PathBuf, pub cuda_home: Option<PathBuf> }` (use the field types `EngineInstallation` already has) and the trait method `fn role_settings(&self) -> Result<RoleSettings, ProviderError>` on `EngineProvider`. `role_installation_as` builds its role-level fields from `role_settings()`. With no installation, `environment_fingerprint` is `"standalone-none"`. `StandaloneApp::profiles()` returns `vec![]`. The banner JSON gains `"profiles": [..]`.

- [ ] **Step 1: Write the failing tests.** Replace the old test in `standalone_start.rs`:

```rust
/// T02, SPEC §8 (amended 2026-10-01): a role with no engine starts, publishes
/// no profile and places nothing; it tells the operator how to add one.
#[tokio::test]
async fn standalone_boots_without_an_engine_and_places_nothing() {
    let dir = safe_state_dir();
    std::env::remove_var("CAPYCTL_VLLM_BIN");
    std::env::remove_var("CAPYCTL_SGLANG_BIN");
    std::env::remove_var("CAPYCTL_TENSORFOLD_BIN");
    std::env::remove_var("CAPYCTL_MODELS_ROOT");
    let app = capyctl_cli::roles::start_standalone_with_config_home(
        dir.path(),
        &dir.path().join(".config"),
    )
    .await
    .expect("a role with no engine starts");
    assert!(app.profiles().is_empty());
    assert_eq!(app.installation_view(), serde_json::Value::Null);
    let _ = app.shutdown().await;
}
```

In `role_text.rs` tests, beside `banner_and_shutdown_in_text`:

```rust
// T02: the banner of a role with no engine names the command that adds one.
#[test]
fn a_banner_with_no_engine_names_engine_add() {
    let text = banner_text(&serde_json::json!({
        "event": "ready", "role": "standalone", "version": "0.1.1", "profiles": []
    }));
    assert!(text.contains("Engines") && text.contains("none") && text.contains("capyctl engine add"), "{text}");
}
```

Copy the other fields the existing banner test passes, so `banner_text` gets the shape it expects.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-cli --test standalone_start -- --test-threads=4 && cargo test -p capyctl-cli --lib role_text`
Expected: FAIL with `NoEngineInstallation` and no `Engines` row.

- [ ] **Step 3: Implement.**
  - In `EnvEngineProvider::installations`, delete the `if all.is_empty()` refusal and return the empty vector. Keep `installation()` (single) as it is: callers that need one installation still get the refusal.
  - Extract `role_settings()` from `role_installation_as` and use it in `start_standalone_in` for `runtime_dir`, ports and models. Replace the `named.first()...ok_or_else(NoEngineInstallation)` block with the settings, and compute `environment_fingerprint` from `named.first()` when one exists, else `"standalone-none"`. Run `grep -n "installation\." crates/capyctl-cli/src/roles.rs` inside `start_standalone_in` and move every role-level use onto the settings.
  - At ~585 (`the host publishes no engine`), change the message to `this role has no engine: register one with \`capyctl engine add <path>\`, then deploy`.
  - Banner: `main.rs` adds `"profiles": app.profiles()` to the ready value. `banner_text` adds an `Engines` row: the profile names joined by `, `, or `none: run \`capyctl engine add <path>\``.
  - `capyctl status` (`client.rs`): when the `/installation` view is null, print `Engine  none: run \`capyctl engine add <path>\`` instead of the installation rows.
  - Remove the "Spec §8: no engine installation, no boot" sentence on `StartError::NoEngineInstallation`. The variant stays for `installation()` callers.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-cli --test standalone_start --test standalone_engines --test role_shutdown -- --test-threads=4 && cargo test -p capyctl-cli --lib`
Expected: PASS.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-cli/src/roles.rs crates/capyctl-cli/src/role_text.rs crates/capyctl-cli/src/main.rs crates/capyctl-cli/src/client.rs crates/capyctl-cli/src/standalone_engines.rs crates/capyctl-cli/tests/standalone_start.rs
git commit -m "feat: start a role with no engine profile"
```

### Task 6: `engine remove` may remove the last profile

**Files:**
- Modify: `crates/capyctl-cli/src/standalone_engines.rs` (`retire_for_removal`, ~376-390: delete the "keeps at least one engine" pre-check; `without`, ~327; `resolve`, ~184: an empty result is a valid publication)
- Modify: `crates/capyctl-cli/src/engine.rs` (`remove`, ~523-565) only if it refuses an empty result locally
- Test: `crates/capyctl-cli/tests/standalone_engines.rs`

**Interfaces:**
- Consumes: Task 5 (an empty profile list boots and publishes).
- Produces: `ControlRequest::Remove` on the last registered profile answers `{"ok": true, "retired": true}`; the following `Add` reload publishes `[]`.

- [ ] **Step 1: Write the failing test** in `standalone_engines.rs`. Boot a standalone document with no `host.local_engine` and no engine variables, and one registered profile. Use `standalone_doc`, `register`, `unregister`, `request` and `deploy`. If `support::boot_configured` always adds a local engine, add `support::boot_registered_only(state, &doc)`, which unsets `CAPYCTL_VLLM_BIN`, `CAPYCTL_SGLANG_BIN` and `CAPYCTL_TENSORFOLD_BIN` for the boot.

```rust
// T02 T16 T32 (ADR 0018 §4, A2): the last registered profile is removed; the
// role keeps running with none, and a profile added again is published live.
#[tokio::test]
async fn the_last_profile_is_removed_and_a_new_one_is_published_live() {
    let state = support::safe_state_dir();
    let document = standalone_doc(state.path());
    register(&document, "only");
    let app = support::boot_registered_only(state.path(), &document)
        .await
        .expect("standalone boots");
    assert_eq!(app.profiles(), vec!["only".to_string()]);
    let socket = state.path().join(SOCKET_NAME);
    let reply = request(&socket, &ControlRequest::Remove { profile: "only".into(), drain: false }, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!({"ok": true, "retired": true}));
    unregister(&document, "only");
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10)).await.unwrap();
    assert_eq!(reload["published"], "published", "{reload}");
    assert!(app.profiles().is_empty());
    register(&document, "again");
    let reload = request(&socket, &ControlRequest::Add, Duration::from_secs(10)).await.unwrap();
    assert_eq!(reload["published"], "published", "{reload}");
    let (status, body) = deploy(&app, state.path(), "m", "again").await;
    assert!(status.is_success(), "{status} {body}");
    let _ = app.shutdown().await;
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p capyctl-cli --test standalone_engines the_last_profile -- --test-threads=4`
Expected: FAIL with `publish_rejected` (`no engine installation: this host declares no engine ...`).

- [ ] **Step 3: Implement.** Delete the "The role keeps at least one engine" pre-check in `retire_for_removal`. With Task 5's `installations` no longer refusing an empty set, `without` and `resolve` succeed on an empty list. Check that the live-reload path (`fn reload`, ~251, `publish_embedded_profiles(.., false)`) accepts an empty list. If the store refuses an empty publication, allow it there with a test in `crates/capyctl-store/src/profile_retirement.rs`'s tests. A deployment on the removed profile drains and stops first through the existing `--drain` path; that is unchanged.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-cli --test standalone_engines --test engine_cli -- --test-threads=4`
Expected: PASS. `standalone_remove_retires_and_the_reload_unpublishes` still refuses removing the environment profile `local` with `invalid_config`.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-cli/src/standalone_engines.rs crates/capyctl-cli/src/engine.rs crates/capyctl-cli/tests/standalone_engines.rs crates/capyctl-cli/tests/support/mod.rs
git commit -m "feat: let engine remove drop the last registered profile"
```

### Task 7: Live row M75 checks "boots with no engine and places nothing"

**Files:**
- Modify: `scripts/live/matrix/rows/M75.sh`
- Modify: `scripts/live/matrix/README.md` (the M75 line ~33 and the table row ~71)

**Interfaces:**
- Consumes: Task 5's banner (`Engines  none`) and JSON ready line (`"profiles": []`).

- [ ] **Step 1: Rewrite the row.** Keep the header shape and change the comment to: `M75 (T02): a role that declares no engine boots with none and places nothing, on each host`. Add `-u CAPYCTL_TENSORFOLD_BIN` to `M75_ENV_UNSET`. `no_engine_boot` now does the following on each host, in a fresh 0700 state directory:
  1. Start `$RBIN start standalone --output json` in the background with the engine variables unset. Wait at most 60 s for the JSON `ready` line in its output file.
  2. Run `$RBIN status --output json` and `$RBIN engine list --output json` against that state directory.
  3. Check with `pgrep -P <role pid>` that no engine child was spawned.
  4. Send SIGTERM to the role and wait at most 30 s for it to exit. Print `exit=$?`.
  5. Remove the directory.

The Python check requires: a ready line with `"profiles": []`; status naming `capyctl engine add`; an engine list with no profiles; no child process; a role exit within the bound. Step names stay `no-engine-$host` and `idle-$host`.

- [ ] **Step 2: Dry-run it**

Run: `bash -n scripts/live/matrix/rows/M75.sh && CAPYCTL_MATRIX_DRY=1 scripts/live/matrix/run_row.sh M75 --no-e0` (check the dry-run switch name in `scripts/live/matrix/lib.sh`; `dry` is the helper the row already calls)
Expected: syntax OK; the dry run prints the commands without contacting a host.

- [ ] **Step 3: Update the README** M75 line to `# a role with no engine boots with none and places nothing`, and update the table text "no-engine refusal (M75)" to "no-engine boot (M75)".

- [ ] **Step 4: Commit.**

```bash
git add scripts/live/matrix/rows/M75.sh scripts/live/matrix/README.md
git commit -m "test: M75 checks that a role with no engine boots and places nothing"
```

---

## Group 4: Client hang-up mid-stream

### Task 8: The forwarder cancels upstream when delivery fails

**Files:**
- Modify: `crates/capyctl-adapters/src/traits.rs`: `enum StreamEnded` (~430) gains `Cancelled`; the `ChatSink` docs (~441-448) and the `ChatForward::forward_chat_stream_async` doc (~456, "stop delivery and drain the backend").
- Modify: `crates/capyctl-adapters/src/forward.rs`: `ChatHttp::stream_inner` (~406-492). On the first failed or timed-out `sink.send`, return `Ok(StreamEnded::Cancelled)` at once and drop the response, which closes the engine connection.
- Modify: `crates/capyctl-testkit/src/fake_engine.rs`: the `ChatForward` impl (~435) returns `Cancelled` when a send fails.
- Modify: every exhaustive `match` on `StreamEnded` (`grep -rn "StreamEnded::" crates`), adding `Cancelled` where the compiler asks. The router mapping is Task 11; until then map `Cancelled` to `LeaseEnd::Uncertain` there, which is today's conservative charge.
- Test: `crates/capyctl-adapters/tests/engine_contract.rs`. Rewrite `async_sink_waits_in_order_and_failure_drains_without_resuming_delivery` (~114) and `draining_stream_reports_backend_progress_after_delivery_failed` (~700).

**Interfaces:**
- Produces: `pub enum StreamEnded { Completed, BackendClosed, Cancelled }`. `Cancelled` means: the sink failed, the forwarder stopped reading and closed the engine connection, and no terminator was observed. Completion is unproven and the engine was asked to abort by the closed socket (SPEC §10 amended).

- [ ] **Step 1: Write the failing tests.** In `engine_contract.rs` add an engine whose SSE body stream records when it is dropped, beside the `engine(sglang, bool, sse)` helper:

```rust
/// An engine streaming `count` chunks 50 ms apart; `dropped` is set when the
/// response body is dropped, which is how the server sees the socket close.
async fn slow_engine(count: usize, dropped: Arc<AtomicBool>) -> String {
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let flag = Flag(dropped.clone());
            async move {
                let body = async_stream::stream! {
                    let _flag = flag;
                    for i in 0..count {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        yield Ok::<_, std::convert::Infallible>(format!("data: {}\n\n", chunk(&format!("t{i}"), None)));
                    }
                    yield Ok("data: [DONE]\n\n".to_string());
                };
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(body))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

// T17 T38, SPEC §10 (amended 2026-10-01): a failed delivery stops reading and
// closes the engine connection; nothing more is delivered or replayed.
#[tokio::test]
async fn a_failed_delivery_cancels_upstream_without_draining() {
    let dropped = Arc::new(AtomicBool::new(false));
    let base = slow_engine(200, dropped.clone()).await;
    let forward = /* build the vLLM forwarder for `base` as the other tests in this file do */;
    let mut sink = FailingAfter { ok: 1, delivered: 0 };
    let started = Instant::now();
    let ended = forward.forward_chat_stream_async(&json!({"model": "m", "stream": true}), &mut sink).await;
    assert!(matches!(ended, Ok(StreamEnded::Cancelled)), "{ended:?}");
    assert_eq!(sink.delivered, 1);
    assert!(started.elapsed() < Duration::from_secs(2), "the stream was not drained");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !dropped.load(Ordering::SeqCst) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dropped.load(Ordering::SeqCst), "the engine saw its connection close");
}

// T17: a stream whose client stays connected still ends on the terminator.
#[tokio::test]
async fn a_connected_stream_still_completes_on_its_terminator() {
    let dropped = Arc::new(AtomicBool::new(false));
    let base = slow_engine(5, dropped).await;
    let forward = /* as above */;
    let mut sink = FailingAfter { ok: usize::MAX, delivered: 0 };
    let ended = forward.forward_chat_stream_async(&json!({"model": "m", "stream": true}), &mut sink).await;
    assert!(matches!(ended, Ok(StreamEnded::Completed)), "{ended:?}");
    assert_eq!(sink.delivered, 5);
}

struct FailingAfter { ok: usize, delivered: usize }
#[async_trait::async_trait]
impl ChatSink for FailingAfter {
    async fn send(&mut self, _chunk: String) -> Result<(), DeliveryFailed> {
        if self.delivered >= self.ok {
            return Err(DeliveryFailed);
        }
        self.delivered += 1;
        Ok(())
    }
}
```

Build `forward` exactly as `async_sink_waits_in_order_and_failure_drains_without_resuming_delivery` builds its vLLM forwarder (same constructor and key handling). Rewrite that test and `draining_stream_reports_backend_progress_after_delivery_failed` to the new contract: after the failure the result is `Cancelled`, and `progressed` is not called again (assert `progressed == delivered + 1`, the failed one included, or whatever the count is at the failure point). Rename them `async_sink_waits_in_order_and_failure_cancels` and `no_progress_is_reported_after_delivery_failed`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-adapters --test engine_contract -- --test-threads=4`
Expected: FAIL to compile (`StreamEnded::Cancelled`), then FAIL on `Completed` after the drain.

- [ ] **Step 3: Implement.** In `stream_inner`, replace the sticky `delivery_failed` drain with:

```rust
// SPEC §10 (amended 2026-10-01): a client that left is not proof the engine
// stopped, so the engine is asked to stop: the connection closes here and
// the lease stays charged until the engine reports quiescence.
if sink_failed {
    return Ok(StreamEnded::Cancelled);
}
```

Dropping `response` at return closes the connection. Make sure the response is not pooled for reuse: reqwest only returns a connection to the pool once its body was read to the end, so dropping it mid-body closes it. Keep the terminator check: a `[DONE]` observed before the failure is still `Completed`. Update the `ChatForward` doc to: `On sink failure or timeout, stop reading and close the backend connection, returning StreamEnded::Cancelled.` Update the `progressed` doc: it is called for backend events read, and no events are read after a failed delivery. FakeEngine: on a failed send `return Ok(StreamEnded::Cancelled)`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-adapters --all-targets -- --test-threads=4 && cargo test -p capyctl-testkit -p capyctl-router --all-targets -- --test-threads=4`
Expected: PASS. Router tests whose own fake `Forward` still drains are unchanged until Task 11.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-adapters/src/traits.rs crates/capyctl-adapters/src/forward.rs crates/capyctl-adapters/tests/engine_contract.rs crates/capyctl-testkit/src/fake_engine.rs crates/capyctl-router/src/stream.rs
git commit -m "feat: close the engine connection when a stream's client hangs up"
```

### Task 9: Adapters report engine-wide quiescence

**Files:**
- Modify: `crates/capyctl-adapters/src/traits.rs` (`EngineAdapter`, after `idle_before_signal`, ~332)
- Modify: `crates/capyctl-adapters/src/vllm/adapter.rs` (`impl EngineAdapter for VllmAdapter`), `sglang/adapter.rs` (`impl EngineAdapter for SglangAdapter`), `tensorfold/adapter.rs` (`impl EngineAdapter for TensorfoldAdapter`)
- Modify (forwarders that wrap another adapter, so the method is not lost behind them): `crates/capyctl-controller/src/installation_gate.rs` (`impl EngineAdapter for InstallationGate`, ~231) and `crates/capyctl-controller/src/checkpoint_digests.rs` (`impl EngineAdapter for CheckpointGate`, ~706; rebase onto Task 2 first)
- Modify: `crates/capyctl-testkit/src/fake_engine.rs` (knob and impl; after Task 8)
- Test: `crates/capyctl-adapters/tests/vllm_residency.rs`, `tests/tensorfold_adapter.rs`, a new SGLang case in the SGLang adapter tests (`grep -ln "idle_from_metrics\|engine_idle" crates/capyctl-adapters/tests`); `crates/capyctl-controller/src/installation_gate.rs` tests

**Interfaces:**
- Produces:

```rust
/// SPEC §10 (amended 2026-10-01): the cancellation acknowledgement for a
/// request whose client hung up. `true` only when the engine's own counters,
/// observed at or after `after_ms` (Unix ms), show no running and no waiting
/// request. Unknown, unreadable or busy is `false`.
async fn engine_quiescent(&self, _member: &MemberRef, _after_ms: i64) -> bool {
    false
}
```

  - vLLM: `matches!(self.http.work_counts().await, Ok(Some((r, w))) if r == 0.0 && w == 0.0)` (gauges `vllm:num_requests_running`, `vllm:num_requests_waiting`). A parked member is quiescent, as in `prepare_park`.
  - SGLang: `crate::sglang::observation::engine_idle(self.base.as_str(), inference_key).await == Ok(true)` with the launch's inference key (gauges `sglang:num_running_reqs`, `sglang:num_queue_reqs`). No key is `false`. Do not use `prepare_park`, which also requires the saver.
  - TensorFold: `self.idle_before_signal(member).await == Some(EngineWork::Idle)` (`requests_running: 0`, `busy: false`).
  - All three read now, so any `after_ms` in the past holds.
  - FakeEngine: `pub fn set_engine_busy(&self, busy: bool)` (a knob, default `false`). `engine_quiescent` returns `!busy`.
  - `InstallationGate` and `CheckpointGate`: delegate to `self.inner.engine_quiescent(member, after_ms)`.

- [ ] **Step 1: Write the failing tests.**
  - vLLM, in `vllm_residency.rs`, using its metrics stub: `/metrics` serving `vllm:num_requests_running 1` then `0`, with `vllm:num_requests_waiting 0`. Assert `false` then `true`. A body missing `vllm:num_requests_waiting` is `false`. Tag `// T17`.
  - SGLang: a loopback stub serving `sglang:num_running_reqs 0` and `sglang:num_queue_reqs 2` is `false`; both `0` is `true`. Tag `// T17 T22`.
  - TensorFold, in `tensorfold_adapter.rs`: `/health` `{"ok":true,"busy":true,"requests_running":0}` is `false`, `{"ok":true,"busy":false,"requests_running":0}` is `true`, not listening is `false`. Tag `// T17 T41`.
  - Wrappers: in `installation_gate.rs` tests, an inner adapter that answers `true` is seen as `true` through the gate:

```rust
// T17: the gate does not hide the engine's quiescence.
#[tokio::test]
async fn the_gate_forwards_engine_quiescence() {
    struct Quiet;
    #[async_trait::async_trait]
    impl EngineAdapter for Quiet {
        /* the required methods as the file's other test adapters write them */
        async fn engine_quiescent(&self, _: &MemberRef, _: i64) -> bool { true }
    }
    let gate = /* build an InstallationGate around Arc::new(Quiet) as the file's tests do */;
    let member = MemberRef { deployment_id: "d".into(), member_id: "b".into() };
    assert!(gate.engine_quiescent(&member, 0).await);
}
```

  Add the same check for `CheckpointGate` in `checkpoint_digests/tests.rs`, with the `Counting` adapter extended to answer `true`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-adapters --all-targets -- --test-threads=4 && cargo test -p capyctl-controller --lib installation_gate checkpoint_digests -- --test-threads=4`
Expected: FAIL (no method, then `false` from the default).

- [ ] **Step 3: Implement** the method and impls above. The vLLM and SGLang reads are bounded by their existing client timeouts (2 s for SGLang `engine_idle`).

- [ ] **Step 4: Run the tests to see them pass** (same commands). Expected: PASS.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-adapters crates/capyctl-controller/src/installation_gate.rs crates/capyctl-controller/src/checkpoint_digests.rs crates/capyctl-controller/src/checkpoint_digests/tests.rs crates/capyctl-testkit/src/fake_engine.rs
git commit -m "feat: adapters report engine-wide quiescence for a cancelled request"
```

### Task 10: Store and lease writer: cancelling leases

**Files:**
- Modify: `crates/capyctl-store/src/schema.rs` (new `SCHEMA_V38` after `SCHEMA_V37`, ~888)
- Modify: `crates/capyctl-store/src/migrations.rs` (`MIGRATIONS`: append `SCHEMA_V38` with comment `// SPEC §10 (2026-10-01): cancellations of hung-up requests.`)
- Modify: `crates/capyctl-store/src/dispatch.rs` (`LeaseWrite`, ~330: add `Cancel(DispatchTicket)`; `apply_request_lease_batch`, ~368)
- Create: `crates/capyctl-store/src/request_lease_cancellations.rs` (`impl Store` read and settle functions; register in `lib.rs`)
- Modify: `crates/capyctl-controller/src/request_leases.rs` (`LeaseEnd`, ~83: add `Cancelling`; `RequestLeaseWriter::close`, ~220)
- Test: tests module in `request_lease_cancellations.rs`; `crates/capyctl-controller/src/request_leases.rs` tests

**Interfaces:**
- Produces:

```sql
-- SCHEMA_V38
CREATE TABLE IF NOT EXISTS request_lease_cancellations(
  lease_id TEXT PRIMARY KEY REFERENCES request_leases(id) ON DELETE CASCADE,
  cancelled_at_ms INTEGER NOT NULL CHECK(cancelled_at_ms>=0)
);
```

```rust
// capyctl-controller
pub enum LeaseEnd { Completed, NotAccepted, Uncertain, Cancelling }
// capyctl-store
pub enum LeaseWrite { /* existing */, Cancel(DispatchTicket) }
pub struct CancellingBinding { pub binding_id: String, pub deployment_id: String, pub leases: usize, pub newest_cancelled_at_ms: i64 }
impl Store {
    /// Bindings of this session with at least one cancelling lease.
    pub fn cancelling_bindings(&self, session: &CoordinatorSession) -> Result<Vec<CancellingBinding>, StoreError>;
    /// Close this session's cancelling leases on the binding cancelled at or
    /// before `asked_at_ms`; journals one event with `receipt`. Returns the count.
    pub fn settle_cancelled_leases(&self, session: &CoordinatorSession, binding_id: &str, asked_at_ms: i64, receipt: &str) -> Result<usize, StoreError>;
}
```

A cancelling lease keeps `disposition='inflight'`. `LeaseWrite::Cancel` inserts the row (`INSERT OR IGNORE`, `cancelled_at_ms = now`) only when the lease exists with that session and `disposition='inflight'`. `settle_cancelled_leases` deletes `request_leases` rows that join a cancellation with `cancelled_at_ms <= asked_at_ms`, `disposition='inflight'`, the current session, and the binding's `(deployment_id, instance_index)` and live generation. Use the same binding-to-lease join as `binding_outstanding_leases` (`ordinary_lifecycle/switching.rs` ~475). The event kind is `request_cancellation_acknowledged`, with `binding_id`, `count` and `receipt`, through `crate::events::append_event`.

- [ ] **Step 1: Write the failing tests** in `request_lease_cancellations.rs` `#[cfg(test)] mod tests`. Build a store with a Ready instance and a granted lease, the way the `switch_outstanding_leases` tests in `ordinary_lifecycle/switching.rs` do; reuse their fixture helper.

```rust
// T17, SPEC §10 (amended): a cancelling lease is still charged: every drain
// that waits for in-flight work waits for it.
#[test]
fn a_cancelling_lease_keeps_every_drain_waiting() {
    let f = ready_instance_with_lease();
    f.store.apply_request_lease_batch(&f.session, vec![LeaseWrite::Cancel(f.ticket.clone())]).unwrap();
    assert_eq!(f.store.switch_outstanding_leases(&f.dep, f.instance, f.generation).unwrap(), 1);
    assert_eq!(f.store.binding_outstanding_leases(&f.binding).unwrap(), Some(1));
    assert_eq!(f.store.cancelling_bindings(&f.session).unwrap()[0].binding_id, f.binding);
}

// T17: only cancellations at or before the quiescence question close.
#[test]
fn settling_closes_only_leases_cancelled_before_the_question() {
    let f = ready_instance_with_lease();
    f.store.apply_request_lease_batch(&f.session, vec![LeaseWrite::Cancel(f.ticket.clone())]).unwrap();
    let at = f.store.cancelling_bindings(&f.session).unwrap()[0].newest_cancelled_at_ms;
    assert_eq!(f.store.settle_cancelled_leases(&f.session, &f.binding, at - 1, "r").unwrap(), 0);
    assert_eq!(f.store.settle_cancelled_leases(&f.session, &f.binding, at, "r").unwrap(), 1);
    assert_eq!(f.store.binding_outstanding_leases(&f.binding).unwrap(), Some(0));
    assert!(f.store.cancelling_bindings(&f.session).unwrap().is_empty());
}

// T17 T38: after a restart the lease is uncertain; its cancellation row
// never closes it.
#[test]
fn a_cancelling_lease_is_uncertain_after_a_restart() {
    let f = ready_instance_with_lease();
    f.store.apply_request_lease_batch(&f.session, vec![LeaseWrite::Cancel(f.ticket.clone())]).unwrap();
    let next = f.store.begin_session(/* as the dispatch.rs session tests do */).unwrap();
    assert!(f.store.cancelling_bindings(&next).unwrap().is_empty());
    assert_eq!(f.store.settle_cancelled_leases(&next, &f.binding, i64::MAX, "r").unwrap(), 0);
    let pending = f.store.pending_dispatches(&f.dep).unwrap();
    assert!(pending.iter().all(|p| p.uncertain), "{pending:?}");
}
```

Adapt the field and function names to the fixture helper you reuse; keep the three assertions. In `request_leases.rs` tests, add `closing_as_cancelling_keeps_the_lease_charged`: open, `close(lease, LeaseEnd::Cancelling)`, and assert one cancelling binding and an outstanding count of 1.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-store --lib request_lease_cancellations -- --test-threads=4 && cargo test -p capyctl-controller --lib request_leases -- --test-threads=4`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** the schema, migration, `LeaseWrite::Cancel`, the two `Store` functions, and `RequestLeaseWriter::close` mapping `LeaseEnd::Cancelling => LeaseWrite::Cancel(ticket)`. Doc on `LeaseEnd::Cancelling`: `SPEC §10 (amended 2026-10-01): the client hung up and the engine connection was closed; the lease stays charged until the engine reports quiescence.` The session-start sweep (`dispatch.rs` ~64) needs no change: it makes the lease `uncertain`, and the settler only reads `inflight` leases.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-store -p capyctl-controller --all-targets --no-fail-fast -- --test-threads=4`
Expected: PASS. Any test pinning `latest_version() == 37` is updated to 38.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-store crates/capyctl-controller/src/request_leases.rs
git commit -m "feat: record a hung-up request's lease as cancelling and keep it charged"
```

### Task 11: The router closes a hung-up stream as cancelling

**Files:**
- Modify: `crates/capyctl-router/src/stream.rs`: module doc (lines 1-4); the comment at ~199-205; the lease-end mapping (~228-232), extracted into `pub fn stream_lease_end`; the comment at ~222-227.
- Test: `crates/capyctl-router/tests/stream_safety.rs` (rewrite `draining_stream_after_disconnect_is_bounded_by_backend_progress` ~501, adjust `client_disconnect_waits_for_backend_completion_before_release` ~266), `crates/capyctl-router/tests/router_stream.rs` (`abandoned_request_keeps_inflight_until_confirmed` ~101: comment only)

**Interfaces:**
- Consumes: `StreamEnded::Cancelled` (Task 8), `LeaseEnd::Cancelling` (Task 10).
- Produces: `pub fn stream_lease_end(result: &Result<Result<StreamEnded, AdapterError>, ()>) -> capyctl_controller::LeaseEnd` (the `bounded` output type; match whatever `bounded` returns).

- [ ] **Step 1: Write the failing tests** in `stream_safety.rs`:

```rust
// T17 T38, SPEC §10 (amended): the lease of a stream whose client hung up
// is cancelling, never completed; a cut stream stays uncertain.
#[test]
fn a_hung_up_stream_closes_its_lease_as_cancelling() {
    use capyctl_controller::LeaseEnd;
    use capyctl_router::stream::stream_lease_end;
    assert_eq!(stream_lease_end(&Ok(Ok(StreamEnded::Cancelled))), LeaseEnd::Cancelling);
    assert_eq!(stream_lease_end(&Ok(Ok(StreamEnded::Completed))), LeaseEnd::Completed);
    assert_eq!(stream_lease_end(&Err(())), LeaseEnd::Uncertain);
}
```

Rewrite the ~501 test as `a_hung_up_stream_without_a_ledger_keeps_its_slot`. Give the file's `Forward` fake a `cancel_on_failure: bool` field (default `false` for the existing tests). With it set, the fake returns `Ok(StreamEnded::Cancelled)` at its first failed `send`, as the real forwarder now does. Drop the response after the first chunk and assert the in-memory count stays `1` for 1 s: no durable ledger means the slot is the only record, as for an uncertain end. Keep `client_disconnect_waits_for_backend_completion_before_release` as it is (a fake that completes still releases on completion), but rename it `a_backend_that_completes_after_a_hang_up_releases_on_completion`. In `router_stream.rs` change the comment in `abandoned_request_keeps_inflight_until_confirmed` to say the guard is held until completion or the engine's quiescence. Check that `LeaseEnd` derives `PartialEq` and `Debug`; add them in Task 10 if not.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-router --test stream_safety -- --test-threads=4`
Expected: FAIL to compile (`stream_lease_end`).

- [ ] **Step 3: Implement.** In `stream.rs`:

```rust
/// SPEC §10 (amended 2026-10-01): the lease closes on the backend's own
/// terminator or on proof the engine never saw the request. A stream whose
/// client hung up was cancelled upstream and stays charged as cancelling
/// until the engine reports quiescence. A stream the router cut for missing
/// its bounds stays uncertain.
pub fn stream_lease_end(result: &Result<Result<StreamEnded, AdapterError>, ()>) -> capyctl_controller::LeaseEnd {
    match result {
        Ok(Ok(StreamEnded::Completed)) => capyctl_controller::LeaseEnd::Completed,
        Ok(Ok(StreamEnded::Cancelled)) => capyctl_controller::LeaseEnd::Cancelling,
        Ok(Err(error)) => crate::chat::lease_end(Some(error)),
        _ => capyctl_controller::LeaseEnd::Uncertain,
    }
}
```

Use it at ~228. The guard handling stays: `Cancelled` is not `Completed`, so the slot is released only when `durable` (the else-if branch), and no `[DONE]` is sent. Replace the module doc's last line with `A client hang-up closes the engine connection; accounting stays charged until the engine reports quiescence (SPEC §10).` Replace the comment at ~199-205 accordingly. Make `stream` a `pub mod` if it is not already exported.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-router --all-targets -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-router
git commit -m "feat: close a hung-up stream's lease as cancelling"
```

### Task 12: The coordinator closes cancelling leases on quiescence

**Files:**
- Modify: `crates/capyctl-controller/src/coordinator/worker.rs`: the tick that runs `advance_preinitialize` and `apply_idle_policy` (~2555-2595). Add a `settle_cancellations(shared)` call before the idle early return, so it runs whether or not idle timers are set.
- Modify: `crates/capyctl-controller/src/remote_execution.rs` (`impl EngineAdapter for RemoteEngine`, ~419)
- Modify: `crates/capyctl-controller/src/load_table.rs` (new `pub fn proves_quiescence`) and `crates/capyctl-controller/src/remote_readiness.rs` (`fn quiescent`, ~505, delegates to it)
- Create: `crates/capyctl-controller/src/coordinator/tests_cancellation.rs` (registered beside `tests_cleanup.rs`)

**Interfaces:**
- Consumes: `EngineAdapter::engine_quiescent` (Task 9); `Store::cancelling_bindings` and `settle_cancelled_leases` (Task 10).
- Produces:

```rust
// load_table.rs
/// SPEC §10 (W12; amended 2026-10-01): a host sample that proves the launch's
/// engine and ingress hold no work: this launch, fresh, taken at or after
/// `after_ms`, nothing in flight at the ingress, the engine scrape read 0
/// running and 0 waiting. Missing gauges are unknown, never zero.
pub fn proves_quiescence(view: &LoadView, host_id: &str, owned_handle: &str, generation: i64, after_ms: i64) -> bool;
```

  - The worker: at most every 250 ms (an `AtomicI64` `cancel_checked_ms` in `Shared`, like `idle_checked_ms`), for each `CancellingBinding`, take its retained `Driver` from `shared.retained` (skip a binding with none), set `asked_at = (shared.clock)()?`, call `driver.engine.engine_quiescent(&MemberRef { deployment_id, member_id: binding_id }, asked_at)`, and on `true` call `settle_cancelled_leases(session, binding, asked_at, receipt)`. Receipts: local `"the engine's own counters read no running and no waiting request after the client hung up"`, remote `"the host reported the launch's engine with 0 running and 0 waiting and 0 in flight at its ingress after the client hung up"`.
  - The returned count marks the tick as changed.
  - `RemoteEngine::engine_quiescent` reads `self.sessions.load_table().sample_at(&InstanceKey::new(dep, generation), now)` and applies `proves_quiescence` with the binding's host id, launch handle and generation. Take them from `self.binding` (the `RemoteLaunchBinding`); if the launch step id is not on it, read it the way `remote_readiness.rs` builds `RemoteReadyLaunch`.

- [ ] **Step 1: Write the failing tests** in `tests_cancellation.rs`. Use the coordinator test harness the `tests_cleanup.rs` tests use (its `CountedIdle` adapter shows how a custom adapter is installed). Use an adapter whose `engine_quiescent` reads an `Arc<AtomicBool>` and records the `after_ms` it was given.

```rust
// T17, SPEC §10 (amended): a cancelling lease closes once the engine reads
// quiescent, and not before.
#[tokio::test]
async fn a_cancelling_lease_closes_on_engine_quiescence() {
    let h = harness_with_ready_instance(Quiet::busy()).await;
    let lease = h.open_lease().await;
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    h.tick_for(Duration::from_millis(600)).await;
    assert_eq!(h.outstanding().await, 1, "busy engine: still charged");
    h.adapter.set_quiet(true);
    h.tick_for(Duration::from_millis(600)).await;
    assert_eq!(h.outstanding().await, 0);
    assert!(h.events().await.iter().any(|e| e.kind == "request_cancellation_acknowledged"));
}

// T17: quiescence is engine-wide; another request keeps it charged.
#[tokio::test]
async fn a_cancelling_lease_waits_for_the_other_requests_on_the_engine() {
    let h = harness_with_ready_instance(Quiet::busy()).await;
    let other = h.open_lease().await;
    let hung_up = h.open_lease().await;
    h.close_lease(hung_up, LeaseEnd::Cancelling).await;
    h.tick_for(Duration::from_millis(600)).await;
    assert_eq!(h.outstanding().await, 2);
    h.close_lease(other, LeaseEnd::Completed).await;
    h.adapter.set_quiet(true);
    h.tick_for(Duration::from_millis(600)).await;
    assert_eq!(h.outstanding().await, 0);
}

// T17: the question is asked after the hang-up; an earlier idle reading does not count.
#[tokio::test]
async fn a_quiescence_sample_before_the_hang_up_closes_nothing() {
    let h = harness_with_ready_instance(Quiet::idle()).await;
    let lease = h.open_lease().await;
    let before = h.now();
    h.close_lease(lease, LeaseEnd::Cancelling).await;
    h.tick_for(Duration::from_millis(600)).await;
    assert!(h.adapter.asked_after().iter().all(|at| *at >= before));
    assert_eq!(h.outstanding().await, 0);
}

// T17: a switch release waits for a cancelling lease, then proceeds.
#[tokio::test]
async fn a_switch_waits_for_a_cancelling_lease() {
    /* as tests_switching.rs drives a release: open a lease, close it as
       Cancelling, request the switch, assert it is still draining after
       600 ms; set_quiet(true); assert the release completes. */
}
```

Write the `harness_with_ready_instance`, `open_lease`, `close_lease`, `tick_for`, `outstanding` and `events` helpers once at the top of the file over the existing coordinator test harness (`grep -n "^async fn\|^fn" crates/capyctl-controller/src/coordinator/tests_cleanup.rs` for the builders). For the switch test, follow the release setup in `tests_switching.rs`. Add a load-table test for `proves_quiescence` in `load_table.rs`'s tests, with a sample older than `after_ms`, a stale sample, `ingress_in_flight = 1`, and `engine: None`, each `false`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p capyctl-controller --lib coordinator::tests_cancellation load_table -- --test-threads=4`
Expected: FAIL (no settler; leases stay charged).

- [ ] **Step 3: Implement** as described in Interfaces. Keep each settle bounded: one adapter call per binding per tick, with no wait loop in the worker.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p capyctl-controller --all-targets --no-fail-fast -- --test-threads=4`
Expected: PASS, including the W12 `remote_readiness` tests through the shared predicate.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-controller
git commit -m "feat: close cancelling leases once the engine reports quiescence"
```

### Task 13: A remote TensorFold load sample counts `/health` too

**Files:**
- Modify: `crates/capyctl-agent/src/load.rs` (the scrape loop, ~414-470; a new pure `fold_tensorfold_health`)
- Test: `crates/capyctl-agent/tests/load.rs`

**Interfaces:**
- Consumes: `capyctl_adapters::tensorfold::http::HealthReport { ok, busy, requests_running }` and `HealthReport::idle() -> Option<bool>` (check that the agent already depends on `capyctl-adapters`; it builds TensorFold launches, so it should).
- Produces: `pub fn fold_tensorfold_health(load: EngineLoad, health: Option<&HealthReport>) -> EngineLoad`. When `health` is not `Some(idle() == Some(true))`, `running` becomes `max(running, 1)`; otherwise `load` is unchanged.

- [ ] **Step 1: Write the failing test** in `tests/load.rs`:

```rust
// T17 T41, ADR 0023 §6 (2026-10-01): a TensorFold sample is quiescent only
// when /health reads idle too.
#[test]
fn a_tensorfold_sample_is_idle_only_when_health_agrees() {
    use capyctl_adapters::tensorfold::http::HealthReport;
    let zero = EngineLoad { running: 0, waiting: 0, kv_usage_ppm: 0 };
    let busy = HealthReport { ok: true, busy: true, requests_running: 0 };
    let idle = HealthReport { ok: true, busy: false, requests_running: 0 };
    assert_eq!(fold_tensorfold_health(zero, Some(&busy)).running, 1);
    assert_eq!(fold_tensorfold_health(zero, None).running, 1);
    assert_eq!(fold_tensorfold_health(zero, Some(&idle)), zero);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p capyctl-agent --test load -- --test-threads=4`
Expected: FAIL to compile.

- [ ] **Step 3: Implement.** In the scrape, when `family_of` names `"tensorfold"`, GET `/health` on the same loopback target within `SCRAPE_TIMEOUT` (no key: TensorFold has none). Parse it as `HealthReport`, then apply `fold_tensorfold_health`. Update the module doc: a sample is a routing hint, except as W12 and SPEC §10 (amended 2026-10-01) quiescence evidence after a restart or a hang-up.

- [ ] **Step 4: Run the tests to see them pass** (same command plus `cargo test -p capyctl-agent --all-targets -- --test-threads=4`). Expected: PASS.

- [ ] **Step 5: Run the verification commands** and commit.

```bash
git add crates/capyctl-agent/src/load.rs crates/capyctl-agent/tests/load.rs
git commit -m "feat: a TensorFold load sample is idle only when its health agrees"
```

### Task 14: End to end on a standalone with a fake vLLM

**Files:**
- Modify: `crates/capyctl-cli/tests/role_shutdown.rs` (`FAKE_VLLM`: log aborted streams; one new test)

**Interfaces:**
- Consumes: Tasks 8, 10, 11, 12. The fake's `/metrics` already reports `vllm:num_requests_running` from its live count, and a write after the client closed raises in Python, so `finally` decrements it.

- [ ] **Step 1: Write the failing test.** In `FAKE_VLLM.chat`, wrap the streaming writes in `try: ... except (BrokenPipeError, ConnectionResetError): open(os.path.join(here, "aborts.log"), "a").write("aborted\n"); return`. Add:

```rust
// T17 T38, SPEC §10 (amended 2026-10-01): a client that hangs up mid-stream
// cancels the engine's request; the lease closes on the engine's own
// counters and a park follows at once instead of after the whole answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hang_up_cancels_the_engine_request_and_park_follows() {
    /* Boot the standalone with the fake vLLM and deploy, as
       standalone_signal_restarts_and_drain_stops_with_cleanup does
       (`deploy(&installation)`, start --wait). */
    // Stream `slower` (120 chunks, about 12 s); read the first chunk, then drop the response.
    // Within 3 s: aborts.log holds one line.
    // Then `capyctl park deployment <name> --wait` (or the role's park API as
    // the file's other tests call it) completes within 5 s of the drop and
    // the deployment reads `parked`.
}
```

Use the file's `chat_request` helper for the streaming request with `"stream": true`, reading with `reqwest` `bytes_stream().next()` before dropping the response. Measure from the drop.

- [ ] **Step 2: Run it to see it fail on a tree without Tasks 8-12** (it passes once they are in). With them merged:

Run: `cargo test -p capyctl-cli --test role_shutdown a_hang_up -- --test-threads=4`
Expected: PASS. Before Task 8 the stream drained, `aborts.log` stayed empty and the park took about 12 s.

- [ ] **Step 3: Run the verification commands** and commit.

```bash
git add crates/capyctl-cli/tests/role_shutdown.rs
git commit -m "test: a hang-up cancels the engine request and park follows"
```

---

## Group 5: Documents

### Task 15: SPEC and ADR amendments

**Files:**
- Modify: `docs/SPEC.md` §8 (the registration paragraph, line ~160), §10 (line ~447), §20 (T02 line ~952, T17 line ~967), §21 "Later amendments" (~1001)
- Modify: `docs/design/adr/0014-deployment-engine-configuration.md` (append Amendment A5; one sentence in §7, ~187-192)
- Modify: `docs/design/adr/0018-engine-registration.md` (append Amendment A2; §4 and §5 unchanged except a pointer)

- [ ] **Step 1: SPEC §10.** Replace `Client disconnect is not proof the engine stopped working. Retain conservative accounting until cancellation acknowledgement, completion observation, or controlled cleanup.` with:

  > Client disconnect is not proof the engine stopped working. *Amended 2026-10-01 (owner decision):* when a client hangs up mid-stream, the router stops reading and closes the engine connection, which vLLM, SGLang and TensorFold treat as an abort. The request stays charged as `cancelling` until the instance's adapter reports engine-wide quiescence, observed after the hang-up: no running and no waiting requests (vLLM and SGLang from their metrics, TensorFold from `/health` `requests_running: 0` and `busy: false`). That report is the cancellation acknowledgement. Park, idle park and switch drains wait for `cancelling` requests as for in-flight ones, and an engine that never reaches quiescence keeps the charge. Retain conservative accounting until cancellation acknowledgement, completion observation, or controlled cleanup.

- [ ] **Step 2: SPEC §8** (end of the registration paragraph, after "CapyCTL still installs no engine."): `*Amended 2026-10-01 (owner decision):* a host or standalone role starts with no engine profile. It publishes an empty profile list, keeps its deployments, places none until a profile exists, and its start banner and status name \`capyctl engine add\`. \`engine remove\` may remove the last registered profile through the same retirement.`

- [ ] **Step 3: SPEC §20.** T02 becomes `Safe files/identity created once; local authenticated listeners; no engine execution; a role with no engine profile boots with none and places nothing (§8, M75).` T17 becomes `Drain honors completion/cancellation; a client hang-up closes the engine connection and stays charged until engine quiescence; no premature park or response replay.`

- [ ] **Step 4: SPEC §21** "Later amendments": `- 2026-10-01, [live follow-ups](specs/2026-10-01-live-followups-design.md): Hugging Face link chains (ADR 0014 A5), a role with no engine (§8, T02, ADR 0018 A2), upstream cancel on client hang-up (§10, T17).` Check the relative link from `docs/SPEC.md`.

- [ ] **Step 5: ADR 0014 Amendment A5** (`## Amendment A5: Hugging Face link chains (owner decision 2026-10-01)`). Cover: the problem (two-hop shared-blob caches refused `unsafe_file`); the rule (at most 8 hops, each resolved lexically against the directory holding the current link, opened from the store descriptor with `O_NOFOLLOW`, inside the store, ending at a regular file; a directory, an escape, a loop or a ninth hop is `unsafe_file`); manifest identity (the first link's path, the final file's identity, so earlier digests are unchanged); the gate wording (`could not be measured (<reason>)` versus `does not match its recorded digest`). In §7 change "Symbolic links are followed only when they resolve inside the host's model store" to "Symbolic links, and chains of up to 8 links (A5), are followed only when every hop resolves inside the host's model store".

- [ ] **Step 6: ADR 0018 Amendment A2** (`## Amendment A2: a role with no engine (owner decision 2026-10-01)`). Cover: a role starts with no profile and publishes an empty list; `engine remove` may remove the last registered profile, retiring it through §4 (deployments drain and stop first); §4's `agent_unreachable` rule stands, so the operator starts the role, removes the profile and stops it again; new deploys still fail fast (§7). Add `(A2: the last profile may be removed.)` at the end of §5's paragraph.

- [ ] **Step 7: Check and commit.**

Run: `grep -n "no engine installation, no boot" -r docs crates` (expect no hits once Task 5 is merged; none in docs now) and `cd site && npm run check` (the SPEC is not synced, but the links check covers `docs/`).

```bash
git add docs/SPEC.md docs/design/adr/0014-deployment-engine-configuration.md docs/design/adr/0018-engine-registration.md
git commit -m "docs: amend SPEC and ADRs for link chains, an empty role and hang-up cancel"
```

### Task 16: Guides, configuration and the 0.1.1 release notes

**Files:**
- Modify: `docs/guide/parking.md` (~87-90, "CapyCTL waits for requests in progress to finish before it parks a model")
- Modify: `docs/guide/engines.md` (~104-107, "or the model sits idle"; the `engine remove` section)
- Modify: `docs/guide/deploy.md` or `docs/guide/configuration.md` (where `validate config` is shown; check with `grep -n "validate config" docs/guide/*.md`)
- Modify: `docs/guide/configuration.md` (the `lifecycle_defaults` block) and `docs/operations/configuration.md` (row ~201)
- Modify: `docs/operations/release-notes-0.1.1.md`
- Regenerate: `site/src/content/docs/docs/**` via `cd site && npm run sync`

- [ ] **Step 1: Parking guide.** After "it never cuts an answer off", add: `When a client hangs up in the middle of an answer, CapyCTL stops the engine's work on it instead of letting it run to the end, and waits until the engine reports no running or waiting requests before it parks or switches.` Add a short "Idle models" note: `CapyCTL stops or parks an idle model only when \`lifecycle_defaults.ready_idle_timeout\` (or \`parked_idle_timeout\` for parked ones) is set; both are off by default.` Show the YAML for standalone (`server.lifecycle_defaults`) and for a server document.

- [ ] **Step 2: Engines guide.** Change "when CapyCTL needs the memory, or the model sits idle," to "when CapyCTL needs the memory, or the model sits idle past `ready_idle_timeout` (off unless set),". In the removal section, add that the last engine can be removed and that the role keeps running with none (`capyctl status` then says `Engine  none: run \`capyctl engine add <path>\``). Add that a Hugging Face cache directory (`snapshots/<rev>`) can be named as the model as it is.

- [ ] **Step 3: `validate config`.** Where the guide shows it, add that without `--host` it checks the document, the timeouts, a declared `memory.startup` and a declared `resources` block, and lists what needs a host. The example output must match the real text from Task 4: run `capyctl validate config <file>` on the guide's example and paste the result.

- [ ] **Step 4: Configuration.** In `docs/guide/configuration.md` next to `lifecycle_defaults`, and in the `docs/operations/configuration.md` row, state: `Unset, a timer is off: no idle model is stopped or parked.`

- [ ] **Step 5: Release notes.** Add to `docs/operations/release-notes-0.1.1.md` under "Other changes":

```markdown
- A client that hangs up in the middle of a streamed answer now stops the
  engine's work on it, for vLLM, SGLang and TensorFold. CapyCTL keeps the
  request counted until the engine reports nothing running or waiting, so a
  park or a switch that follows no longer waits for the whole answer.
- A model named by its Hugging Face cache directory (`snapshots/<rev>`) is
  measured even when its files link twice, as recent `huggingface_hub`
  versions store them. A checkpoint that cannot be measured now says why,
  instead of reporting a digest mismatch.
- `capyctl validate config` without `--host` checks a `resources` block and
  refuses a TensorFold deployment without one, and lists what still needs a
  host.
- A role with no engine starts and says how to add one; `capyctl engine
  remove` can remove the last engine.
- Idle models are stopped or parked only when `ready_idle_timeout` or
  `parked_idle_timeout` is set; both are off by default.
```

- [ ] **Step 6: Sync and check.**

Run: `cd site && npm run sync && npm run check`
Expected: PASS (links, commands, voice).

- [ ] **Step 7: Commit.**

```bash
git add docs/guide docs/operations/configuration.md docs/operations/release-notes-0.1.1.md site/src/content/docs
git commit -m "docs: hang-up cancel, link chains, offline validation, empty role, idle timers off by default"
```

---

## Live

### Task 17: Live qualification and the status entry

Run this after every group is merged into the branch and the local checks pass. Only one live session runs on the hosts at a time. Host names, addresses and home paths are never written into the repository; evidence goes into the commit message and the status runbook in prose. Never read or print engine keys: observe engine load through `capyctl status ... --output json` and the role log, not the engine's keyed endpoints. TensorFold's `/health` has no key.

**Files:**
- Modify: `docs/runbooks/f2-current-status.md` (new entry at the top; change the "Found and not fixed" paragraph of the TensorFold entry to point at it)

**Interfaces:**
- Consumes: everything above; `scripts/live/matrix/sync.sh`, `scripts/live/matrix/run_row.sh`, the untracked `hosts.local.env` (`HOST_B_VLLM_VENV_DIR`, `SGLANG_VENV_DIR`), host B's `~/tensorfold-0.6.0-venv` and `~/tensorfold-spike/hf`.

- [ ] **Step 1: Preconditions.** On the control machine run `scripts/live/matrix/sync.sh snapshot && scripts/live/matrix/sync.sh push a && scripts/live/matrix/sync.sh push b && scripts/live/matrix/sync.sh build a && scripts/live/matrix/sync.sh build b`. Then on each host check that no capyctl role is running (`pgrep -a capyctl`; stop a running service with its own CLI) and that no GPU process exists (`nvidia-smi --query-compute-apps=pid --format=csv,noheader` is empty). Use the built binary and a fresh state directory per session, as in the TensorFold plan's Task 14 Step 1.

- [ ] **Step 2: TF2 with an unmodified Hugging Face cache (host B, standalone).** Use `export CAPYCTL_MODELS_ROOT=~/tensorfold-spike/hf` and `capyctl engine add ~/tensorfold-0.6.0-venv`. Deploy `nemotron` with `model:` set to the snapshot directory as `huggingface_hub` left it (`find ~/tensorfold-spike/hf -path '*snapshots*' -name config.json -printf '%h\n'`), `context_length: 32768` and the guide's `resources`.
  Run: `capyctl deploy model --file nemotron.yaml && capyctl start deployment nemotron --wait`, then a plain and a streaming chat as in the requests guide.
  Expected: the digest is measured (no `unsafe_file`), the deployment is ready, and both requests answer. Also check `find <snapshot> -type l -exec readlink {} \; | head` to confirm the cache really is the two-hop layout.

- [ ] **Step 3: TF5 on TensorFold (host B).** Send a streaming request to `nemotron` with `max_tokens: 2048` and close the client after the first chunks (`curl -N ... | head -c 2000`). Poll `curl -s http://127.0.0.1:<engine port>/health` (port from `capyctl inspect deployment nemotron --output json`).
  Expected: `busy: false` and `requests_running: 0` within a few seconds of the close, not after 2048 tokens (TF5 before: 17.2 s). The role log shows the request's lease closing on quiescence (`request_cancellation_acknowledged`). Then `capyctl start deployment qwen --evict --wait` (or the TF4 vLLM deployment): the release follows without waiting for the old answer. Record the close-to-idle and close-to-released times.

- [ ] **Step 4: Hang-up then park on vLLM 0.29 (host B).** Run `capyctl engine add ~/$HOST_B_VLLM_VENV_DIR` and deploy the TF4 Qwen3-4B (`model: {hf: Qwen/Qwen3-4B}`, `engine: vllm`), then `start --wait`. Stream `max_tokens: 2048` and close after the first chunks. Poll `capyctl status deployment qwen --output json` for the engine running count and the in-flight count (check the field names in the output).
  Expected: running reaches 0 within a few seconds of the close, and the in-flight count reaches 0. Then `capyctl park deployment qwen --wait` completes promptly and the deployment reads `parked`. Record both times.

- [ ] **Step 5: Hang-up then park on SGLang 0.5.20 (host A, then host B).** Register `~/$SGLANG_VENV_DIR` with `capyctl engine add` and deploy the smallest instruct model `scripts/live/matrix/models.json` lists for the host's SGLang fixtures. Then repeat Step 4.
  Expected: as Step 4, on both hosts.

- [ ] **Step 6: Empty role and the last profile (host B, standalone).** Stop the role with a drained shutdown. Start it again, run `capyctl engine remove` on every registered profile (`--drain` where a deployment uses it), and check that `capyctl status` names `capyctl engine add`. Stop the role and start it with no engine.
  Expected: it boots, its banner shows `Engines  none: ...`, and `engine add` publishes live.

- [ ] **Step 7: M75 on both hosts.**
  Run: `scripts/live/matrix/run_row.sh M75 --no-e0`
  Expected: both hosts pass: boots with no engine, no profiles, no engine child, clean exit.

- [ ] **Step 8: Clean up.** Stop every role with a drained shutdown, delete the deployments, remove the profiles, and confirm no capyctl, engine or GPU process is left on either host. Keep the downloaded models unless the owner asks for them to be removed, and remove the temporary state directories.

- [ ] **Step 9: Status entry.** At the top of `docs/runbooks/f2-current-status.md`:

```markdown
## Live follow-ups — <date>

The four problems the TensorFold run found are fixed
(`docs/specs/2026-10-01-live-followups-design.md`): Hugging Face link chains
are measured (ADR 0014 A5) and an unmeasurable checkpoint says why;
`validate config` without `--host` checks `resources`; a role starts with no
engine and `engine remove` takes the last profile (ADR 0018 A2); a client
hang-up mid-stream closes the engine connection and the request stays
charged until the engine reports quiescence (SPEC §10).

Live: TF2 on an unmodified two-hop cache <result>; TF5 <idle N s after the
close, released N s later>; vLLM 0.29 on host B <running 0 after N s, parked
after N s>; SGLang 0.5.20 on host A <...> and host B <...>; empty role and
last-profile removal <result>; M75 <result on both hosts>. Local checks:
formatting, Clippy with warnings denied, core suite <n> passed / 0 failed,
workspace <n> / 0, site check. CPU and Fake-engine tests are not
qualification.
```

Fill every `<...>` with the measured value. A row that failed is written as failed, with its reason, and the task is not done until it passes or the owner accepts the failure. In the TensorFold entry, replace the "Found and not fixed" paragraph with `Fixed in the live follow-ups entry above.`

- [ ] **Step 10: Commit.**

```bash
git add docs/runbooks/f2-current-status.md
git commit -m "docs: live follow-ups qualified on host A and host B"
```

---

## Self-Review

**Spec coverage.**
- §1 link chains:
  - walker: Task 1 (8 hops, per-hop resolution, `O_NOFOLLOW` from the store, containment, regular file, loop, ninth hop, directory, escape)
  - manifest identity: Task 1 (digest equal to a plain copy)
  - gate wording: Task 2
  - ADR 0014 §7: Task 15
- §2 offline validation:
  - typed decoding and intrinsic and claim checks: Task 3
  - TensorFold refusal: Tasks 3 and 4
  - text output naming what needs a host: Task 4
- §3 empty role:
  - boot, empty publication, banner and status: Task 5
  - last-profile removal through retirement: Task 6
  - ADR 0018 §4 unchanged and A2: Task 15
  - M75 and T02 text: Tasks 7 and 15
- §4 hang-up:
  - forwarder stops and closes: Task 8
  - `cancelling` and still charged: Tasks 10 and 11
  - quiescence per engine: Task 9 (local) and Tasks 12 and 13 (remote)
  - drains wait: Task 10 (by construction, tested) and Task 12 (switch)
  - unchanged connected streams and no replay: Tasks 8 and 11
  - SPEC §10: Task 15
  - live checks: Task 17
- Testing: every task is written to fail first and carries its acceptance tags. TF2 on an unmodified cache, TF5, vLLM and SGLang hang-up-then-park, and M75 are in Task 17.
- Docs: Task 16 covers the guides, configuration, release notes and idle-off-by-default.

**Placeholder scan.** The remaining `/* ... */` markers in test code point at named existing helpers whose constructor shape the implementer must copy (forwarder construction in `engine_contract.rs`, the coordinator harness in `tests_cleanup.rs`, the switch setup in `tests_switching.rs`, the standalone boot in `role_shutdown.rs`). Each names the file and the test to copy from, and the assertions are given in full.

**Type consistency.** These names are the same in every task that uses them: `StreamEnded::Cancelled`, `LeaseEnd::Cancelling`, `LeaseWrite::Cancel`, `engine_quiescent(&MemberRef, i64) -> bool`, `cancelling_bindings`, `settle_cancelled_leases`, `CancellingBinding`, `proves_quiescence`, `fold_tensorfold_health`, `validate_declared_resources`, `role_settings`/`RoleSettings`, `stream_lease_end`, and `MAX_LINK_HOPS`.
