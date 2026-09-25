# Declared Park Tiers Implementation Plan

**Goal:** Make a deployment's park tier a declared, host-validated part of its configuration, so an invalid combination is refused at configuration time instead of parking successfully and freeing nothing.

**Architecture:** The deployment's `residency` names the tier. The host declares, per memory domain, whether device and host memory are one physical pool. `resolve_effective` rejects a tier the profile or the host cannot deliver. Residency then derives each engine's park strategy at the place that engine takes it: SGLang's are startup flags and become launch settings; vLLM's level is a parameter of the sleep call and stays out of launch settings.

**Tech Stack:** Rust, `mllm-config` (schema and effective resolution), `mllm-domain` (launch settings), `mllm-store` (residency gate), `mllm-cli` (standalone's published policy).

**Spec:** `docs/design/adr/0010-declared-park-tiers.md`

## Global Constraints

- Prose in code comments, commit messages and documents is normal English, regardless of chat style.
- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// SPEC §6.2: restart-only is first-class`.
- Tag tests with their acceptance-matrix ID (`// T14`) where one applies.
- Core suite command: `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`
- Clippy must pass with `-D warnings` across those crates.
- `crates/mllm-cli/tests/live_interactive.rs` and `.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-2-report.md` are excluded: do not read, edit, format, test or stage them.
- This plan makes the tier declarable and validated. It does **not** implement ordinary park, and does not decide when the park/restore proof runs. Both are out of scope.

---

### Task 1: The host declares each domain's memory topology

A host-backed park retains weights in host RAM. Where device and host memory are one physical pool, that frees nothing. Nothing in `HostPolicy` records which kind of host this is — `unified` in standalone's policy is a name `standalone_config.rs` chose, not a declared property. Task 4 needs this fact; this task adds it.

**Files:**
- Modify: `crates/mllm-config/src/effective.rs` (add `DomainMemory`, add field to `DomainPolicy` at 309-314 and `RawDomain` at 417-422)
- Modify: `crates/mllm-config/src/effective/core.rs:129-138` (domain normalization)
- Modify: `crates/mllm-config/src/schema.rs:144-149` (`DOMAIN` field spec)
- Test: `crates/mllm-config/tests/effective.rs`
- Modify: `crates/mllm-config/tests/fixtures/*.json` (every host document, and three goldens)

**Test helper.** `crates/mllm-config/tests/effective.rs` already has the builder every
test in this plan uses:

```rust
fn fixture() -> (serde_json::Value, serde_json::Value) {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    (all["deployment"].clone(), all["host"].clone())
}
```

It returns `(deployment, host)`. Every test below mutates those values rather than
building documents from scratch.

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum DomainMemory { Unified, Distinct }` and `DomainPolicy::memory: DomainMemory`, both in `mllm_config::effective`, plus the `host_with_domain_memory` test helper. Task 4 reads `memory` and reuses the helper; Task 5 sets the field in standalone's published policy.

- [ ] **Step 1: Write the failing test**

Add to `crates/mllm-config/tests/effective.rs`, and add `DomainMemory` to its
`use mllm_config::effective::{...}` list at the top:

```rust
/// Set the topology of the lab host's only domain. Used by this task and Task 4.
fn host_with_domain_memory(memory: &str) -> serde_json::Value {
    let (_, mut host) = fixture();
    host["resource_policy"]["domains"]["unified"]["memory"] = memory.into();
    host
}

/// A host-backed park retains weights in host RAM, which frees nothing where that
/// is the same pool the device allocates from. The host is the only party that
/// knows which it is, so it states it rather than having it guessed from a domain's
/// name or from which limits happen to be set.
#[test]
fn a_domain_declares_whether_its_memory_is_one_pool() {
    let (deployment, _) = fixture();
    for (declared, expected) in [
        ("unified", DomainMemory::Unified),
        ("distinct", DomainMemory::Distinct),
    ] {
        let host = host_with_domain_memory(declared);
        let resolved = resolve_effective(&deployment, &host).expect("valid host");
        assert_eq!(resolved.host.domains["unified"].memory, expected, "{declared}");
    }
}

/// Omitting it is a configuration error, not a default. Either default is wrong on
/// one class of hardware, and the failure it causes is silent: a park that frees
/// nothing and an eviction that does not relieve pressure.
#[test]
fn a_domain_without_declared_memory_is_rejected() {
    let (deployment, mut host) = fixture();
    host["resource_policy"]["domains"]["unified"]
        .as_object_mut()
        .expect("the domain is an object")
        .remove("memory");
    let error = resolve_effective(&deployment, &host).expect_err("must be rejected");
    assert!(format!("{error}").contains("memory"), "{error}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --offline -p mllm-config --test effective a_domain_declares_whether_its_memory_is_one_pool`
Expected: FAIL to compile with `cannot find type DomainMemory in this scope`.

- [ ] **Step 3: Add the type and the field**

In `crates/mllm-config/src/effective.rs`, beside `DomainPolicy`:

```rust
/// Whether a domain's device memory and host memory are one physical pool.
///
/// On a unified-memory host such as a GB10, retaining a weight backup "in host RAM"
/// allocates from the same pool the device allocates from, so it frees nothing. Only
/// the operator registering the host knows this; it must not be inferred from a
/// domain's name or from which limits are set. SPEC §6.2's host-backed park is
/// meaningful only where this is `Distinct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainMemory {
    Unified,
    Distinct,
}
```

Add the field to `DomainPolicy`:

```rust
pub struct DomainPolicy {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub host_kv_limit: Option<i64>,
    pub parked_limit: Option<i64>,
    pub memory: DomainMemory,
}
```

Add it to `RawDomain`, with no `Option` and no `serde(default)` so that omitting it fails deserialization:

```rust
struct RawDomain {
    managed_limit: String,
    free_reserve: String,
    host_kv_limit: Option<String>,
    parked_limit: Option<String>,
    memory: DomainMemory,
}
```

In `crates/mllm-config/src/effective/core.rs`, in the domain loop at 129-138:

```rust
        let value = DomainPolicy {
            managed_limit: parse_bytes(&raw.managed_limit)?,
            free_reserve: parse_bytes(&raw.free_reserve)?,
            host_kv_limit: raw.host_kv_limit.as_deref().map(parse_bytes).transpose()?,
            parked_limit: raw.parked_limit.as_deref().map(parse_bytes).transpose()?,
            memory: raw.memory,
        };
```

In `crates/mllm-config/src/schema.rs`, extend `DOMAIN` at 144-149:

```rust
    const DOMAIN: FieldSpec = FieldSpec::Struct(&[
        ("managed_limit", BYTES),
        ("free_reserve", BYTES),
        ("host_kv_limit", BYTES),
        ("parked_limit", BYTES),
        ("memory", SCALAR),
    ]);
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --offline -p mllm-config --test effective a_domain`
Expected: both tests PASS.

- [ ] **Step 5: Add the field to every host document, then to the goldens**

Two different edits, in this order.

*Inputs.* Every fixture carrying a `resource_policy.domains` map now fails to
deserialize without `memory`. Find them and add `"memory": "unified"` to each domain —
all existing fixtures describe the lab host, whose single domain is `unified`:

```bash
grep -rln 'resource_policy' crates/mllm-config/tests/fixtures/
```

*Goldens.* `DomainPolicy` is `Serialize` and the effective document embeds
`host.domains`, so the three `effective-*-golden.json` files gain the key in their
expected output too. Add `"memory": "unified"` inside
`effective.host.domains.unified` in each.

The `qualification_fingerprint` in those goldens does **not** change: the fingerprint
hashes `host_devices`, not `domains` (`effective/core.rs:322-347`). If a fingerprint
does change, something other than this task changed with it — stop and read the diff.

Edit by hand. Several goldens are compared as parsed JSON but are stored formatted;
an automated rewrite that reorders keys makes the diff unreadable even when it passes.

- [ ] **Step 6: Run the config suite**

Run: `cargo test --offline -p mllm-config`
Expected: PASS, no failures.

- [ ] **Step 7: Commit**

```bash
git add crates/mllm-config/src/effective.rs crates/mllm-config/src/effective/core.rs \
        crates/mllm-config/src/schema.rs crates/mllm-config/src/effective/tests.rs \
        crates/mllm-config/tests/fixtures
git commit -m "feat(config): a host declares each domain's memory topology

A host-backed park retains weights in host RAM, which frees nothing where that is
the same pool the device allocates from. Nothing recorded which kind of host this
was: the name unified in standalone's policy is a label, not a declared property.

The field is required rather than defaulted. Either default is wrong on one class
of hardware, and the resulting failure is silent - a park that frees nothing and an
eviction that does not relieve pressure."
```

---

### Task 2: Residency names the tier

`Residency` is `Warm | RestartOnly`, which cannot say *which* warm. SPEC §6.2 distinguishes a host-backed park, which retains a weight backup in host RAM, from deep parking, which releases weights and re-reads them on wake. ADR 0010 adopts those two as named tiers and deliberately does not adopt `auto`, because selecting a tier at runtime is the ladder the ADR rejects.

**Files:**
- Modify: `crates/mllm-config/src/effective.rs:155-160` (the enum) and `:621` (the sleep-mode check)
- Modify: `crates/mllm-config/src/effective/candidate.rs:1121-1122` (case-count rules)
- Modify: `crates/mllm-store/src/ordinary_lifecycle.rs:455` (the residency gate)
- Test: `crates/mllm-config/src/effective/tests.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `pub enum Residency { RestartOnly, HostBacked, Deep }` in `mllm_config::effective`, serialized as `restart_only`, `host_backed`, `deep`. Tasks 3, 4 and 5 match on it.

- [ ] **Step 1: Write the failing test**

Add to `crates/mllm-config/tests/effective.rs`, and add `Residency` to its
`use mllm_config::effective::{...}` list:

```rust
/// SPEC §6.2 distinguishes a host-backed park, which retains a weight backup in host
/// RAM, from deep parking, which releases the weights. A single `warm` cannot say
/// which, and the two differ in wake cost by several times and in host RAM by orders
/// of magnitude, so the deployment names the one it wants.
#[test]
fn residency_names_which_park_the_deployment_asks_for() {
    for (declared, expected) in [
        ("restart_only", Residency::RestartOnly),
        ("host_backed", Residency::HostBacked),
        ("deep", Residency::Deep),
    ] {
        let (mut deployment, host) = fixture();
        deployment["residency"] = declared.into();
        // Task 4 refuses host_backed on a unified domain, which is what the lab host
        // declares, so this asserts the vocabulary on a host that allows every tier.
        let mut host = host;
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let resolved = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        assert_eq!(resolved.residency, expected, "for {declared}");
    }
}

/// `auto` is deliberately not adopted: choosing a tier at runtime is the fallback
/// ladder ADR 0010 rejects, and SGLang cannot implement one because its memory-saver
/// and weights-CPU-backup are startup flags.
#[test]
fn residency_auto_is_refused() {
    let (mut deployment, host) = fixture();
    deployment["residency"] = "auto".into();
    assert!(
        resolve_effective(&deployment, &host).is_err(),
        "auto must not resolve"
    );
}
```

The `f2-deployment.json` fixture declares a vLLM profile with
`enable_sleep_mode: true`, so both parking tiers satisfy the check at
`effective.rs:621`.

Write this test before Task 4 exists, so the `memory` line is inert for now and
correct once Task 4 lands. That ordering is deliberate: the alternative is a test
that passes in Task 2 and silently starts failing in Task 4.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --offline -p mllm-config --test effective residency_names_which_park`
Expected: FAIL to compile with `no variant named HostBacked found for enum Residency`.

- [ ] **Step 3: Replace the enum and its four call sites**

In `crates/mllm-config/src/effective.rs:155-160`:

```rust
/// Which park a deployment asks for, per SPEC §6.2.
///
/// `auto` from the spec is deliberately absent: selecting a tier at runtime is a
/// fallback ladder, and SGLang cannot implement one because its memory-saver and
/// weights-CPU-backup are startup flags that a running engine cannot acquire.
/// `deep_required` is absent for the same reason it is unnecessary — a declared tier
/// the profile or host cannot deliver is already a validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    /// Stop and initialize again. First-class behaviour, including for backends with
    /// no qualified memory-release API.
    RestartOnly,
    /// Weights retained in host RAM, KV dropped. Frees nothing where device and host
    /// memory are one pool, which Task 5's host check refuses.
    HostBacked,
    /// Weights and KV released; weights re-read from the checkpoint on wake.
    Deep,
}

impl Residency {
    /// Whether this tier parks at all, as opposed to stopping and starting again.
    pub fn parks(self) -> bool {
        matches!(self, Self::HostBacked | Self::Deep)
    }
}
```

At `crates/mllm-config/src/effective.rs:621`, the vLLM sleep check becomes:

```rust
            if residency.parks() && !value.enable_sleep_mode {
                return Err(invalid(
                    "runtime_profiles.launch_settings.enable_sleep_mode",
                    "a parking vLLM deployment requires sleep mode",
                ));
            }
```

At `crates/mllm-config/src/effective/candidate.rs:1121-1122`, the case-count rules key on whether the recipe parks, because a parking recipe is the one that must exercise a park and a restore:

```rust
        || (residency.parks() && cases.len() < 10)
        || (residency == Residency::RestartOnly && cases.len() != 5)
```

At `crates/mllm-store/src/ordinary_lifecycle.rs:455`, the gate keeps its exact meaning — a deployment that never parks needs no qualification — and reads better as the positive question:

```rust
    if !e.residency.parks() {
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --offline -p mllm-config --test effective residency`
Expected: both tests PASS.

- [ ] **Step 5: Move every fixture off `warm`, and expect fingerprints to change**

Run: `grep -rln '"residency" *: *"warm"' crates/ docs/`

Each occurrence describes a vLLM or SGLang profile that releases memory, which is
`deep`. Replace `"warm"` with `"deep"` in each. Do not touch
`docs/examples/server.yaml`: its `residency: exclusive_per_pool` is a different field
about pool sharing, not a park tier.

**The `qualification_fingerprint` in the goldens will change.** `residency` is one of
the fields hashed (`effective/core.rs:322-347`), so renaming the value is a recipe
change. That is correct and is what SPEC §8.4 requires — *"Changes invalidate the
affected evidence"* — and no recorded qualification exists in any store today, so
nothing is being invalidated in practice. Take the new fingerprint from the test
failure output and paste it into the golden; do not hand-compute it.

- [ ] **Step 5a: Re-run and update the goldens**

Run: `cargo test --offline -p mllm-config --test effective ordinary_engine_compatibility_goldens 2>&1 | head -40`

The assertion prints the whole resolved document against the golden. Update each
golden's `residency` and `qualification_fingerprint` from that output, then re-run
until it passes.

- [ ] **Step 6: Run the affected suites**

Run: `cargo test --offline -p mllm-config -p mllm-store`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/mllm-config crates/mllm-store/src/ordinary_lifecycle.rs
git commit -m "feat(config): residency names which park a deployment asks for

SPEC 6.2 distinguishes a host-backed park, which retains a weight backup in host
RAM, from deep parking, which releases the weights and re-reads them on wake. A
single warm could not say which, though they differ several-fold in wake cost and
by orders of magnitude in host RAM.

auto is deliberately not adopted. Choosing a tier at runtime is a fallback ladder,
and SGLang cannot implement one: its memory-saver and weights-CPU-backup are
startup flags a running engine cannot acquire. deep_required is unnecessary because
a declared tier that cannot be delivered is already a validation failure."
```

---

### Task 3: SGLang's launch flags derive from the declared tier

`SglangLaunchSettings` already carries `memory_saver`, `cpu_weight_backup` and `weight_restore`, but `effective.rs:658-680` sets them to constants: `memory_saver: true`, `cpu_weight_backup: false`, `weight_restore: "disk_reload"`. Nothing in configuration reaches them, so SGLang's tier is fixed regardless of what the deployment asks for. These are startup flags, so they are exactly where residency must land for SGLang.

**Files:**
- Modify: `crates/mllm-config/src/effective.rs:658-680`
- Test: `crates/mllm-config/tests/effective.rs`

**Interfaces:**
- Consumes: `Residency::{RestartOnly, HostBacked, Deep}` and `Residency::parks()` from Task 2.
- Produces: nothing new; changes the values of existing `SglangLaunchSettings` fields.

- [ ] **Step 1: Write the failing test**

```rust
/// SGLang takes its park strategy as startup flags, so the declared tier has to
/// reach them. They were constants, which meant a deployment asking for a
/// host-backed park launched an engine that could only deep-park.
#[test]
fn sglang_launch_flags_follow_the_declared_tier() {
    let expected = [
        // (residency, memory_saver, cpu_weight_backup, weight_restore)
        ("restart_only", false, false, "disk_reload"),
        ("host_backed", true, true, "cpu_backup"),
        ("deep", true, false, "disk_reload"),
    ];
    for (declared, memory_saver, cpu_weight_backup, weight_restore) in expected {
        let (mut deployment, mut host) = fixture();
        deployment["residency"] = declared.into();
        // Same profile rewrite `ordinary_engine_compatibility_goldens` uses to point
        // the lab host at SGLang.
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["args"] = serde_json::json!([]);
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
        profile["launch_settings"] = serde_json::json!({
            "engine": "sglang",
            "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
            "requested_budget": {"kv_cache_bytes": "4GiB", "static_memory_fraction_bps": 7500}
        });
        // host_backed needs a host whose pools are distinct; Task 4 refuses it here
        // otherwise, and this test is about the flags, not the host check.
        host["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
        let resolved = resolve_effective(&deployment, &host)
            .unwrap_or_else(|error| panic!("{declared} must resolve: {error}"));
        let ProfileLaunchSettings::Sglang(settings) = &resolved.profile.launch_settings else {
            panic!("expected SGLang launch settings for {declared}");
        };
        assert_eq!(settings.memory_saver, memory_saver, "memory_saver for {declared}");
        assert_eq!(
            settings.cpu_weight_backup, cpu_weight_backup,
            "cpu_weight_backup for {declared}"
        );
        assert_eq!(
            settings.weight_restore, weight_restore,
            "weight_restore for {declared}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --offline -p mllm-config --test effective sglang_launch_flags_follow_the_declared_tier`
Expected: FAIL on the `restart_only` row first — `memory_saver` is the constant `true`
while the test expects `false`.

- [ ] **Step 3: Derive the flags from residency**

Replace the three constants in `crates/mllm-config/src/effective.rs:658-680`:

```rust
            // SGLang takes its park strategy at launch: --enable-memory-saver and
            // --enable-weights-cpu-backup cannot be added to a running engine. The
            // declared tier therefore has to reach the launch settings, unlike
            // vLLM's level, which is a parameter of the sleep call.
            let memory_saver = residency.parks();
            let cpu_weight_backup = residency == Residency::HostBacked;
            let weight_restore = if cpu_weight_backup {
                // --enable-weights-cpu-backup, available since SGLang v0.5: weights
                // are copied to pinned host memory on sleep and restored from there.
                "cpu_backup"
            } else {
                "disk_reload"
            };
            ProfileLaunchSettings::Sglang(SglangLaunchSettings {
                recipe,
                tensor_parallel_size: 1,
                data_parallel_size: 1,
                tokenizer_workers: 1,
                model_dtype: "bfloat16".into(),
                context_tokens: 4096,
                max_running_requests: 8,
                max_total_tokens: 4096,
                prefill_cuda_graphs: false,
                decode_cuda_graphs: false,
                memory_saver,
                cpu_weight_backup,
                speculative_decoding: false,
                lora: false,
                trust_remote_code: false,
                disaggregation: false,
                external_cache: false,
                cpu_kv_offload: false,
                native_grpc: false,
                weight_restore: weight_restore.into(),
                requested_budget: budget,
            })
```

`residency` is already a parameter of this function; no signature change is needed.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --offline -p mllm-config --test effective sglang_launch_flags_follow_the_declared_tier`
Expected: PASS.

- [ ] **Step 5: Confirm the SGLang golden is unchanged**

`crates/mllm-config/tests/fixtures/effective-sglang-golden.json` declares
`"residency": "deep"` after Task 2. Deep parking derives
`memory_saver: true, cpu_weight_backup: false, weight_restore: "disk_reload"`, which
are the values the golden already carries, so it should not move.

Run: `cargo test --offline -p mllm-config`
Expected: PASS with no further golden edits. If the SGLang golden does change, the
fixture was describing something other than deep parking — read the diff and
understand it before accepting it.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-config
git commit -m "feat(config): SGLang's park flags follow the declared tier

memory_saver, cpu_weight_backup and weight_restore were constants, so a deployment
asking for a host-backed park launched an engine that could only deep-park. These
are startup flags that a running engine cannot acquire, which is why the declared
tier has to reach them and why no runtime fallback is possible."
```

---

### Task 4: A parking deployment is refused on a host that cannot deliver its tier

With Tasks 1-3 in place the tier is declared and reaches the engine, but a host-backed park on a unified-memory domain is still accepted and still frees nothing. This is the check the whole plan exists for.

**Files:**
- Modify: `crates/mllm-config/src/effective.rs` (in `resolve_effective`, after the deployment's domains are known)
- Test: `crates/mllm-config/tests/effective.rs`

**Interfaces:**
- Consumes: `DomainMemory` from Task 1, `Residency` from Task 2.
- Produces: nothing; adds a validation failure.

- [ ] **Step 1: Write the failing test**

```rust
/// A host-backed park retains weights in host RAM. Where that is the same pool the
/// device allocates from, it frees nothing: the park reports success, the memory is
/// still held, and the eviction it was meant to enable does not relieve pressure.
/// Refusing at configuration time is the only point where that is visible.
#[test]
fn a_host_backed_park_is_refused_on_a_unified_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "host_backed".into();
    let host = host_with_domain_memory("unified");

    let error = resolve_effective(&deployment, &host).expect_err("must be refused");
    let text = format!("{error}");
    assert!(text.contains("unified"), "names the domain: {text}");
}

/// The same deployment is valid where the pools are distinct - that is the hardware
/// the tier exists for.
#[test]
fn a_host_backed_park_resolves_on_a_distinct_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "host_backed".into();
    let host = host_with_domain_memory("distinct");
    resolve_effective(&deployment, &host).expect("host-backed is valid where pools differ");
}

/// Deep parking releases the weights, so it is valid on either topology.
#[test]
fn deep_parking_resolves_on_a_unified_domain() {
    let (mut deployment, _) = fixture();
    deployment["residency"] = "deep".into();
    let host = host_with_domain_memory("unified");
    resolve_effective(&deployment, &host).expect("deep parking releases, so it is valid");
}
```

`host_with_domain_memory` is the helper added in Task 1.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --offline -p mllm-config --test effective a_host_backed_park_is_refused_on_a_unified_domain`
Expected: FAIL with `must be refused: Ok(..)`.

- [ ] **Step 3: Add the check**

In `resolve_effective`, after the effective deployment's allocations are resolved and before it is returned, add:

```rust
    // SPEC §6.2's host-backed park retains a weight backup in host RAM. Where a
    // domain's device and host memory are one pool, that allocates from the pool it
    // is supposed to free, so the park succeeds and releases nothing. The failure is
    // otherwise silent, which is why it is refused here rather than at first park.
    if deployment.residency == Residency::HostBacked {
        for allocation in resources
            .values()
            .flat_map(|footprint| footprint.allocations.iter())
        {
            let domain = host.domains.get(&allocation.domain).ok_or_else(|| {
                invalid("resources", "allocation names a domain the host does not declare")
            })?;
            if domain.memory == DomainMemory::Unified {
                return Err(invalid(
                    "residency",
                    &format!(
                        "host_backed retains weights in host memory, but domain \
                         '{}' declares device and host memory as one pool, so it \
                         would free nothing; use deep or restart_only",
                        allocation.domain
                    ),
                ));
            }
        }
    }
```

Adapt the iteration to however `resolve_effective` holds the resolved footprints at that point — the requirement is that every domain any phase allocates from is checked, not only one phase. If `invalid` takes `&'static str` for its second argument rather than a `String`, add a sibling constructor that takes an owned message rather than leaking the formatted string.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --offline -p mllm-config --test effective park_is_refused && cargo test --offline -p mllm-config --test effective parking_resolves`
Expected: all three PASS.

- [ ] **Step 5: Run the config and store suites**

Run: `cargo test --offline -p mllm-config -p mllm-store`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-config
git commit -m "feat(config): refuse a host-backed park where memory is one pool

A host-backed park retains weights in host RAM. On a unified-memory host that
allocates from the pool it is meant to free, so the park reports success, the
memory is still held, and the eviction it was meant to enable does not relieve
pressure. Nothing later in the lifecycle can detect that, which is why it is
refused at configuration time with the domain named."
```

---

### Task 5: Standalone declares its topology and its tier

Standalone publishes the only host policy the product creates. It must now declare each domain's memory topology, and its deployments must name a tier that the GB10 can actually deliver.

**Files:**
- Modify: `crates/mllm-cli/src/standalone_config.rs:57-65` (host policy domains) and `:104-105` (deployment residency)
- Test: `crates/mllm-cli/src/standalone_config/tests.rs`

**Interfaces:**
- Consumes: `DomainMemory` from Task 1, `Residency` from Task 2, the check from Task 4.
- Produces: a host policy carrying `memory`, unchanged otherwise.

- [ ] **Step 1: Write the failing test**

Add to `crates/mllm-cli/src/standalone_config/tests.rs`:

```rust
/// The hardware standalone runs on has one physical pool, which is what the domain
/// name has always claimed and nothing has ever stated. Declaring it is what makes
/// a host-backed park refusable rather than silently useless.
#[test]
fn the_published_host_declares_one_memory_pool() {
    let host = host_policy("fake", "/bin/true", "fp", false, 1 << 40);
    assert_eq!(
        host["resource_policy"]["domains"]["unified"]["memory"],
        "unified"
    );
}

/// Standalone's deployments stay restart-only until ordinary park exists. The point
/// of asserting it is that the value is now one of three rather than one of two, so
/// a later change to a parking tier is a deliberate edit with a test behind it.
#[test]
fn a_standalone_deployment_is_restart_only() {
    let deployment = deployment_document("m", "m", "/models/m", 1 << 40);
    assert_eq!(deployment["residency"], "restart_only");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --offline -p mllm-cli --lib the_published_host_declares_one_memory_pool`
Expected: FAIL with `assertion left == right failed, left: Null, right: "unified"`.

- [ ] **Step 3: Declare the topology**

In `crates/mllm-cli/src/standalone_config.rs`, in the `resource_policy.domains` map:

```rust
        "resource_policy": {
            "domains": {
                DOMAIN: {
                    "managed_limit": share(MANAGED_FRACTION),
                    "free_reserve": share(FREE_RESERVE_FRACTION),
                    "parked_limit": share(PARKED_FRACTION),
                    "host_kv_limit": share(HOST_KV_FRACTION),
                    // One physical pool: a weight backup "in host RAM" would
                    // allocate from the memory it is meant to free, so ADR 0010's
                    // check refuses a host-backed park against this host.
                    "memory": "unified"
                }
            },
```

The deployment's `"residency": "restart_only"` at line 105 is already correct and needs no change; the comment above it should now read:

```rust
        // SPEC §6.2's fallback. Ordinary park is not implemented, so no parking tier
        // can be declared yet; ADR 0010 makes the choice expressible.
        "residency": "restart_only",
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --offline -p mllm-cli --lib standalone_config`
Expected: PASS.

- [ ] **Step 5: Run the CLI suite end to end**

Run: `cargo test --offline -p mllm-cli`
Expected: PASS, including `a1_gate`, `stop_intent`, `standalone_lifecycle` and `roles_f1`. These boot standalone and publish this policy, so they are the real check that the new required field is present everywhere it must be.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat(cli): standalone declares its memory topology

The hardware has one physical pool, which the domain name has always claimed and
nothing ever stated. Declaring it is what lets ADR 0010's check refuse a
host-backed park here instead of accepting one that would free nothing."
```

---

### Task 6: Verify the whole workspace and record the outcome

**Files:**
- Modify: `docs/runbooks/f2-current-status.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Run the core suite**

Run:

```bash
cargo test --offline -p mllm-adapters -p mllm-store -p mllm-controller \
  -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4
```

Expected: PASS. If `qualification_progression::ordinary_cleanup_races_ready_completion_and_duplicate_accept_and_arm` fails, re-run that target alone before treating it as a regression — it is a known fixture flake under concurrent workspace load and passes in isolation.

- [ ] **Step 2: Run the remaining crates**

Run: `cargo test --offline -p mllm-config -p mllm-cli -p mllm-router -p mllm-domain -p mllm-agent -p mllm-launchers`
Expected: PASS.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --offline --workspace --all-targets -- -D warnings`
Expected: no output.

- [ ] **Step 4: Record the outcome in the status runbook**

Under A1b in `docs/runbooks/f2-current-status.md`, add:

```markdown
- [x] Make the park tier declarable and host-validated (ADR 0010). Residency names
      the tier (`restart_only`, `host_backed`, `deep`); hosts declare each domain's
      memory topology; a host-backed park is refused on a one-pool domain; SGLang's
      startup flags follow the declared tier. This does not implement park.
```

- [ ] **Step 5: Commit**

```bash
git add docs/runbooks/f2-current-status.md
git commit -m "docs: record that the park tier is declarable and validated"
```

---

## What this plan deliberately leaves undone

**Ordinary park is not implemented.** The tier is declarable and validated; nothing parks yet. That is A1b's remaining work and needs its own plan.

**The proof is not wired.** ADR 0010 decision 6 says evidence records what the declared tier achieved — tier reached, measured wake duration, whether the model generated afterwards. There is nowhere to record it until a park exists, and when the park/restore proof runs is an open owner decision.

**The vLLM level is not yet passed at park time.** Residency determines it, but the call site is in ordinary park, which does not exist. The existing derivation from `ParkPolicy::ExperimentalAllowed` lives in `crates/mllm-controller/src/operations.rs`, which has no production caller and is deleted by A1's legacy-authority removal.

**Revocation on repeated wake failure is not built.** ADR 0010 decision 8 defers to
SPEC §13.2: keep admission closed on wake failure, allow a bounded clean restart after
verified cleanup, and disable a profile's parking capability after repeated failures.
There is no park to fail yet, so there is nothing to revoke.

**Native parking still blocked.** `VllmAdapter` has no `execute_persisted`; `ProfileBindings` refuses SGLang because nothing resolves its admin credential or supplies a trusted observation socket. Neither is touched here.
