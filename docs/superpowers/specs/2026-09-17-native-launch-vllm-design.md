# Native launch through the coordinator, vLLM first

**Date:** 2026-09-17
**Status:** Approved by the owner in design review, then revised after a six-reviewer
document review on 2026-09-17 (§11 records each decision). Awaiting the owner's read of
the revision, then the implementation plan.
**Slice:** S1 of the F2 exit roadmap in §1.
**Governs:** how the coordinator starts, stops and fails a real engine process, and the
shape every later slice builds on.

## 1. Why, and the roadmap this belongs to

On 2026-09-17 the owner rejected a reviewed, CPU-green branch as incomplete: no native
engine can start through the production path. The coordinator's only launch call is
`EngineAdapter::execute_persisted`. `VllmAdapter` does not implement it, `ProfileBindings`
refuses SGLang, and only the Fake engine has ever run end to end. Stop on a real engine
would also never finish, because ordinary cleanup only observes absence and nothing sends
a signal.

The owner's acceptance bar is the F2 exit gate (SPEC §18): both engines side by side, full
initialization and offloading, and the residency scenarios active/active, active/parked,
parked/active and parked/parked, live on `host-a`. That is seven slices, each with
its own design, plan, live run and review:

| Slice | Deliverable | Live proof |
|---|---|---|
| **S1** | vLLM launches, serves, stops and fails through the coordinator | restart-only cycle and two failure cases |
| **S1r** | Restart re-attach: on startup mllm takes back, or stops, every engine it recorded | kill mllm under a serving engine, restart, keep serving |
| **S2** | Ordinary park and wake, vLLM, wired to `mllm-domain/src/park.rs` | active to parked to active, serving after wake |
| **S3** | SGLang launches through the same mechanism | restart-only cycle on SGLang |
| **S4** | Park and wake, SGLang | active to parked to active |
| **S5** | Eviction in the authority, coexistence, the scenario matrix; the legacy F1 `Controller` in `operations.rs` is deleted here, which closes SPEC §18's "no parallel controller implementation" clause | the four residency pairs and pressure-driven switching |
| **S1b** | Model source materialization (`huggingface`, `http`) into the host model store; nothing in the gate needs it, so it follows the gate | deploy from a repo reference |

Merges to `main` happen after S1 and S3 are both live-green (S1r and S2 land between
them in order), and again after S5. S1b follows the second merge.

**Parking is a requirement, and so are its protections.** The owner's ruling: parking is
required product behavior, not an optional experiment. On `host-a` device and host
memory are one pool, so ADR 0010 refuses `host_backed` there and `deep` is the only tier
the box can deliver. For vLLM, deep parking runs through sleep mode, which needs vLLM's
development mode; vLLM's own security documentation advises against that in production
because it also enables the collective RPC control (SPEC §9.1). mllm therefore treats
three protections as requirements rather than mitigations: the engine listens on the
loopback address only, every launch has its own engine key that only mllm holds (§3),
and no user-reachable route ever forwards an engine control path. S2 proves all three
live with a parked engine, and the claim at S5 is stated in those words: parking works
under these tested conditions. A vLLM control path that does not need development mode
is later work. SGLang's memory-release path is not tied to a development mode and is
not affected the same way. The owner also ruled on the switch: **deep parking is on by
default and a host opts out**, the reverse of what SPEC §9.1, T21 and `AGENTS.md` say
today. Those three texts are amended by the S2 ADR, which owns parking; until then the
code carries the new default and the documents carry a pointer to this decision. The
consequence for S1: vLLM launches with sleep mode enabled from the first slice, so the
launch shape S1 proves live is the one S2 parks, and the three protections are
exercised, not deferred. A host that opts out launches without sleep mode and gets
restart-only parking.

Decisions taken in this review that bind later slices:

- **SGLang keys (S3).** The admin and inference keys belong to the engine installation
  (ADR 0008). mllm generates them when an installation is registered, stores them in
  SQLite encrypted, and passes them to the process at launch through the private
  descriptor the wrapper already reads. The encryption key lives in
  `<state_dir>/identity/`, owner-only, generated at first boot, never in the database.
  `credential_ref` on a profile becomes a real reference into that table. Re-registering
  an installation generates a fresh pair and invalidates the stored one; rotation takes
  effect at the next launch. The storage mechanism itself lands in S1, because the
  per-launch engine key (§3) needs it first.
- **Post-launch retry (after S3).** Deferred until real failure samples exist; see §6.

## 2. The pattern: director and builder

The coordinator is the director. It picks the builder for the engine family a profile
names, hands it the tools it needs, calls the steps in order, records each result, and
decides nothing about how an engine is driven. The adapter is the builder: one object per
engine family, constructed with its tools, executing each step completely, including
spawning processes when the step is Initialize. The director never touches a process. The
builder never touches the database.

SPEC §8.3 already divides the work this way: launch, terminate and handle inspection
belong to the launcher; reservation policy, retries, timeouts and fairness belong to the
controller; an adapter does not decide those.

One constraint shapes the tools. A process identity must be durable before the process
runs (SPEC §13.2, T33), so that a crash can never leave an engine mllm cannot name. The
builder therefore spawns through a tool the director supplies. `DurableSpawn` already
implements the mechanism: the child starts gated, a callback records its identity, and
only then is the gate released. The callback is the director's; the builder only calls
the tool.

## 3. Launch tools and builder construction

Crate direction is `mllm-domain` ← `mllm-adapters` ← `mllm-launchers` ← `mllm-controller` ←
`mllm-cli`. The adapter cannot hold `DurableSpawn` directly, so it holds a trait object
defined in `mllm-adapters` and implemented in `mllm-launchers`.

New trait in `crates/mllm-adapters/src/traits.rs`:

```rust
/// Process tools a builder uses on Initialize and Cleanup. The director supplies
/// them; the builder never learns where identities are recorded.
pub trait OwnedProcessLaunch: Send + Sync {
    /// Spawn gated: the child runs only after its identity is durable.
    fn spawn_durable(&self, incarnation: &str, cmd: &RenderedCommand)
        -> Result<ProcessIdentity, RuntimeError>;
    /// Live now, with the same start identity: boot id and start ticks, not pid alone.
    fn present(&self, identity: &ProcessIdentity) -> Presence;
    /// Every live member of the process group the recorded API process led: the API
    /// process first when it is still live, workers named `worker-0`, `worker-1`, ...
    /// in start order, and an empty list when no member is live. Empty is an answer,
    /// not an error; cleanup depends on it.
    fn observe_group(&self, api: &ProcessIdentity)
        -> Result<Vec<ProcessIdentity>, RuntimeError>;
    /// SIGTERM the owned group, wait `grace`, SIGKILL, then prove every identity gone
    /// and the group itself empty.
    fn terminate_owned(&self, identities: &[ProcessIdentity], grace: Duration)
        -> Result<(), RuntimeError>;
}
```

The trait is synchronous on purpose; `mllm-launchers` takes no async runtime. The builder
calls `terminate_owned` and its blocking waits through `tokio::task::spawn_blocking`, so
the coordinator's timeout and shutdown signal stay able to interrupt a slow stop and no
worker thread is held for the length of a grace period.

`Presence` moves from `mllm-launchers/src/process_absence.rs` to
`mllm-domain/src/completion.rs`, beside `ProcessIdentity`, so both crates can name it.
The older `Launcher` trait and `OwnedHandle` stay for the legacy F1 Controller; nothing
new uses them.

`crates/mllm-launchers/src/owned_launch.rs` implements it as
`DurableProcessLaunch { spawn: DurableSpawn, association: Arc<dyn LaunchAssociation> }`.
`spawn_durable` calls `spawn_persisted`; an outcome with
`initialization_acknowledged: false` is an error carrying the reason, and the gated child
is never released. Two changes to `DurableSpawn` come with it:

- **Output goes to a file.** Today it sends the child's stdout and stderr to null, so
  the log the failure path quotes would never exist. It opens the plan's `engine_log`
  (created `0600`, parent directories `0700`) and redirects both streams into it before
  the gate is released, as `ExecLauncher` already does for `MLLM_ENGINE_LOG`. It must be
  a file and never a pipe held by mllm: a pipe would kill the engine when mllm exits,
  and S1r depends on engines outliving mllm.
- **A child that is never released is disposed of.** Today `detach_reaper` moves the
  write end of the gate into a thread that waits on the child, so the pipe never closes
  and a blocked `sh` is left behind; the crate's own test has to kill it by hand. When
  the gate is not released, `spawn_durable` closes the gate, signals the gated child's
  process group, waits for it, and only then returns the error. A failed launch leaves
  no process behind.

`terminate_owned` reuses `ExecLauncher`'s existing signal escalation, and `observe_group`
wraps `observe_process_group` from `crates/mllm-launchers/src/group_observation.rs`,
which is extended to report an empty group when the leader is gone instead of failing
closed. Neither operation is written a second time. §5 specifies both.

The association is built in the coordinator's driver factory (`spawn_resolved` in
`crates/mllm-controller/src/coordinator/worker.rs`) per `InitializeWork`, capturing the
shared owner state; `persist_api_identity` calls
`Store::record_api_identity(session, fence, binding_id, identity)`.

`AdapterSpec::Vllm` gains `launch: Option<VllmLaunchPlan>`, which is data only.
`resolve(declared, spec, tools: Option<Arc<dyn OwnedProcessLaunch>>)` hands the tool to
the vLLM builder. `ProfileBindings::new(clock, log_dir)` builds the plan from the frozen
effective configuration and nothing ambient:

| Plan field | Source |
|---|---|
| `engine_bin` | `profile.executable` |
| `engine_path_extra` | parent directory of `profile.executable` |
| `model_path` | the resolved model source (§7) |
| `port` | parsed from `work.endpoint()` |
| `served_model_name` | `effective.routes[0]` |
| `granted` | `launch_settings.requested_budget` |
| `engine_args` | `profile.args` |
| `sleep_flags` | `--enable-sleep-mode` and its companions when `launch_settings.enable_sleep_mode`, which is on unless the host opts out; `VLLM_SERVER_DEV_MODE=1` follows it |
| `api_key` | `None` for the renderer, so `--api-key` is never emitted; the key travels in the environment, see below |
| `tensor_parallel_size`, `pipeline_parallel_size`, `kv_cache_dtype`, `block_size_tokens`, `cpu_offload_bytes` | `launch_settings`, all five |
| `engine_log` | `<state_dir>/logs/<deployment>/<incarnation>.log` |

`render_command` in `crates/mllm-adapters/src/vllm/args.rs` gains the two flags mllm
reserves and does not yet emit: `--host 127.0.0.1` always, and `--served-model-name` from
the new `PlanInputVllm.served_model_name`. It also renders the five launch settings the
schema already validates and nothing passes on today: `--tensor-parallel-size`,
`--pipeline-parallel-size`, `--kv-cache-dtype`, `--block-size`, and `--cpu-offload-gb`
(converted from bytes with the unit stated, only when above zero). mllm reserves all
five flags, so a profile could never have supplied them; without this an accepted
setting would be silently ignored. One unit test pins each flag. Profile arguments
render after them and are validated by `validate_profile_args` as today.

**The park switch is an opt-out, and it is one switch.** `security.experimental_controls`
on a profile becomes `security.deep_park: enabled | disabled`, default `enabled`. It does
two things together: `ProfileBindings` maps it to the adapter's park policy, and
`disabled` forces `sleep_flags` empty and `VLLM_SERVER_DEV_MODE=0` regardless of
`launch_settings.enable_sleep_mode`, so an opted-out host never launches a
development-mode engine. Effective-config validation refuses `deep_park: disabled` on a
deployment whose residency parks, with a message naming both settings. The host-level
`enable_sleep_mode` (`MLLM_DEEP_PARK` for standalone) therefore never overrides a
profile that says `disabled`. The existing T21 test flips meaning: the default permits
the park controls, an explicit `disabled` refuses them without an engine call.

**`--trust-remote-code` becomes a host opt-in.** It is on vLLM's approved pass-through
list today and lets a model directory execute its own Python at load. It stays passable
only when the engine installation sets `security.trust_remote_code: true`, default
`false`; a profile argument list that carries it without the switch is refused. Qwen3
does not need it.

**The engine key.** SPEC §13.3: local-only does not mean unauthenticated. For every
launch mllm generates a random 32-byte key and delivers it to vLLM through the child's
environment as `VLLM_API_KEY`, never on the command line: the renderer leaves
`PlanInputVllm.api_key` as `None`, and the builder inserts `VLLM_API_KEY`, `PATH` with
the engine's own `bin` first, and the log path into the rendered environment after
rendering, the way `VllmAdapter::render_plan` assembles them today. The existing
`MLLM_ENGINE_API_KEY` name in `render_plan` is retired; L3 proves the engine actually
requires the key, so a name mismatch cannot launch an unauthenticated engine silently.
The forwarder sends the key on every request. The loopback bind stays as a second
control.

**The control routes are guarded by mllm, because vLLM does not.** Verified on
`host-a` on 2026-09-17: vLLM 0.29's `AuthenticationMiddleware` guards only paths
under `/v1`, `/v2`, `/inference` and `/cohere`. The development-mode routes `/sleep`,
`/wake_up`, `/is_sleeping` and `/collective_rpc` are unauthenticated, and development
mode is required for parking, which the owner ruled is on by default. So the guard is
a requirement and lands in S1: `runtime/mllm_vllm_guard.py` is a small ASGI middleware
that requires `Authorization: Bearer <VLLM_API_KEY>` on every path except `/health`.
Whenever development mode is on, mllm renders `--middleware
mllm_vllm_guard.RequireEngineKey` and sets `PYTHONPATH` to its runtime directory in the
child environment; both are owned by mllm, a profile can neither pass nor remove them.
The runtime directory is the same one the SGLang wrapper lives in; standalone reads
`MLLM_RUNTIME_DIR`, default the `runtime/` directory beside the binary's source
checkout. L3 proves the guard live. A Unix-socket listener (`--uds`) is a later option
that would also exclude other local users; it needs a Unix-socket HTTP client in the
router and is not S1.

The key is stored with the binding, encrypted, so that S1r can rebuild an engine handle
from stored facts alone. Construction: XChaCha20-Poly1305 (`chacha20poly1305` crate,
the workspace's first authenticated-encryption dependency), a fresh 24-byte nonce from
the OS random source per row, and `binding_id || incarnation` as associated data, so a
row copied between bindings does not authenticate. A row that fails to authenticate is
a hard error that leaves the binding `Uncertain`. Schema v14, forward-only, adds the
table: ciphertext and nonce per binding. The key row is deleted in the same transaction
that releases the binding, on ordinary cleanup and on `release_failed_launch`, so the
stored secret set is exactly the set of engines that exist. The encryption key is a
32-byte file in `<state_dir>/identity/`, owner-only, generated at first boot and never
written to the database; the database alone cannot recover an engine key, and a
restored database without its identity directory reaches S1r's unprovable branch. The
SGLang installation keys in S3 use the same construction in an installation-scoped
table, keyed by installation and revision rather than by binding; they are not rows of
this table.

**The router must reach the engine the coordinator launched.** Today `roles.rs` builds
the forwarding table once at boot from a fixed port, and `LifecyclePort` exposes no
endpoint or key. With leased ports and per-launch keys that table cannot be right.
`LifecyclePort` gains a projection, `runtime_endpoint(deployment) -> Option<{endpoint,
engine_key}>`, answered from the coordinator's retained runtime, and the router builds
its forwarder per request from it; the boot-time `RouterDeps.forwards` table is
removed. The forwarder forwards only its chat and models paths and refuses any other
upstream path, which is the control L3 drives directly.

**Requested budget cannot exceed the admitted footprint.** `PlanInputVllm.granted` is
rendered from `launch_settings.requested_budget`, which is a request. Deploy-time
validation refuses a deployment whose profile requests more KV than the Ready-phase
allocation it declares, so the engine's pool is never sized above what admission
accounted for.

**One store precondition changes.** The association writes the API identity into
`runtime_bindings.identities_json` before the engine runs, and `record_launch` in
`crates/mllm-store/src/ordinary_lifecycle.rs` today requires that column to still be
`'[]'`; as written, every native start would end `Conflict` with the engine running.
`record_launch` also accepts a binding that holds exactly the API identity
`record_api_identity` wrote for the same binding and incarnation.

Unchanged: the `EngineAdapter::execute_persisted` signature and the ordering of the
worker's `drive`. Store changes in S1 are exactly: the `record_launch` precondition above,
the worker-identity rule in §4, the encrypted engine-key table (schema v14), and §6's
new transition.

## 4. The vLLM Initialize step

`VllmAdapter::execute_persisted` for `RuntimeAction::Initialize`, with context
`identities: OwnedLaunch`, a Ready completion target and vLLM launch settings. Every wait
is bounded by `context.deadline_ms` and by the coordinator's Initialize bound.

**That bound is not `protocol_timeout`.** `drive` caps every adapter call at
`min(deadline - now, protocol_timeout)`, `protocol_timeout` defaults to 30 s, and
standalone takes `CoordinatorOptions::default()`. This project measured a 4B cold start
at 27 to 63 s. As written, mllm would give up on a healthy engine mid-load and then kill
it. `CoordinatorOptions` gains `initialize_timeout`, default 15 minutes, validated
between 30 s and 2 h, and `drive` uses it in place of `protocol_timeout` for Initialize.
Standalone constructs its options explicitly. The builder's own waits end 2 s before
`context.deadline_ms`, so its "deadline reached with the process present" error reaches
the coordinator before `drive`'s timeout drops the effect future; a coordinator-side
timeout that fires anyway is handled by §6 the same way.

1. **Guard.** `Unsupported` if no plan or tool is attached, the launch settings are not
   vLLM, or this adapter already launched this binding and incarnation. One launch per
   incarnation.
2. **Render.** `render_command(&plan)`. The environment carries `PATH` with the engine's
   own `bin` first, the log path, and `VLLM_SERVER_DEV_MODE` as today.
3. **Spawn.** `tools.spawn_durable(incarnation, &cmd)` returns the API process identity
   or an error.
4. **Readiness.** Poll `check_readiness` every 500 ms (SPEC §6.1: the served name appears
   in `/v1/models`; liveness of an HTTP server is not readiness). Between polls call
   `tools.present(&identity)`. A process that is gone ends the step with an error that
   carries the last 20 lines of the engine log. A deadline reached with the process
   present ends it with an error saying so.
5. **Probe.** One `forward_chat`, `max_tokens: 8`, `temperature: 0`. Empty content or an
   HTTP error ends the step with an error.
6. **Enumerate.** `tools.observe_group(&identity)`. vLLM is not one process: the API
   process starts an `EngineCore` worker that holds the model in device memory. Record
   the API process and every worker.
7. **Observe.** Return `EffectObservation` with those identities, a receipt naming the
   fingerprint and endpoint, and the facts a cold start proves: `AllocationsRestored`,
   `WeightsUsable`, `CacheValid`, `ModelUsable`.

The coordinator then records the owned launch, whose first identity matches the one
already stored by the association, and completes the step. The retained `Driver` keeps
the adapter for Cleanup.

Recording the workers is not optional. The store's `canonical_members` and `members`
(`crates/mllm-store/src/lifecycle/completion.rs`) accept exactly two identities, `api`
and `worker-0`, so a start that reports one identity is refused; the domain's
`verify_completion` already accepts `api` plus any number of `worker-` roles with a
shared boot id and distinct pids and needs no change. And the release proof covers only
recorded identities: with the worker unrecorded, mllm could release a grant while
`EngineCore` still holds the memory. The store rule is generalized to match the
domain's, which a tensor-parallel launch needs anyway.

Park effects return `Unsupported` from the vLLM builder until S2.

## 5. Termination and cleanup

Today every resolved adapter's cleanup closure is `observed_gone`: read `/proc`, report.
For native builders the closure becomes:

1. `tools.terminate_owned(&context.identities, grace)`. For each identity, check presence
   first. Already gone needs no signal. Present with matching boot id and start ticks is
   signalled. The same pid with a different start identity is refused as `Uncertain`; mllm
   never signals a process it cannot prove it owns (SPEC §13.2). `SIGTERM` goes to the
   process group of the API process, which `DurableSpawn` made a group leader. Poll
   `verify_gone` every 200 ms until `grace`; then `SIGKILL` and poll for a fixed 5 s.
2. `observed_gone(&context, &clock)`, which now also requires `observe_group` to find
   the group empty, so a child that was never recorded cannot hide behind the proof.
   `AllGone` with an empty group yields `CleanupEvidence`. Anything else is an error;
   ownership and the reservation are retained.

`CoordinatorOptions.terminate_grace` defaults to 15 s. The cleanup closure is bounded by
the smaller of the store's cleanup deadline and `protocol_timeout`, so validation rejects
any grace where `terminate_grace + 5 s` is not strictly less than `protocol_timeout`,
with a floor of 1 s. A grace that cannot fit would turn every Stop into a timeout.

The closure is chosen by the builder the factory resolved: a builder that launches
processes terminates and then inspects; one that launches nothing only inspects. The
store keeps arming `CleanupMode::TerminateOwned` as today, and `InspectOwnedGone` stays
reserved for S1r. Both take identities from the context, never from adapter memory.

**Until S1r lands,** a coordinator restart strands a running engine: `drive_cleanup`
refuses a binding the worker does not retain, and nothing ties the engine's life to
mllm's. A stranded engine is cleared by hand, and the live runner prints the recorded
process identities to make that exact. S1r removes the gap. On startup, for each engine
the store says was running: a different boot id means it is gone, so release and start
again if the deployment wants to be ready; the same boot, the recorded pid with the
recorded start ticks, that process proven to own the listening socket on the recorded
endpoint (so the key is never sent to whatever else took the port), and an engine that
answers there with its key and lists the served model means re-attach, by rebuilding
the handle from stored facts; a port held by anything else is treated as gone; a stored
key that fails to decrypt or authenticate is treated as present but unprovable;
present but not answering means terminate, prove gone, release and start again; gone
means prove, release and start again; indeterminate keeps the reservation, reads
`Uncertain`, and is surfaced. S1's obligation to S1r is that every engine handle can be
rebuilt from what the store holds: identities, endpoint, served name, launch settings
and the encrypted engine key.

## 6. Launch failure in v1: terminate, prove, release, fail

`RuntimeError` has no failed variant, and once a step is armed the worker turns any error
into `Uncertain` and pauses until an operator sends Stop. A vLLM that dies on a bad model
path would wait for a human. The coordinator instead classifies by proof, not by the
builder's opinion. After any Initialize error on a native builder:

1. Read the binding's recorded identities from the store. None recorded means the gate
   never opened: `spawn_durable` disposed of the gated child before returning (§3) and
   no engine ran. That case skips step 2, because `verify_gone` on an empty set returns
   `Indeterminate` by design ("no recorded processes" could be a lost record). Here it
   is not a lost record: the never-released spawn outcome is itself the evidence, and
   the flow goes straight to step 3 with an empty identity set.
2. Otherwise `terminate_owned`, then `verify_gone` and an empty group.
3. **Proven gone.** New store transition
   `release_failed_launch(session, step_id, evidence, now)`, one transaction: the step
   becomes `cancelled` and its lifecycle run and operation `failed` (the schema's step
   states have no failed value and its run states do, so no migration is needed), the
   binding becomes `released`, the endpoint lease and resource grant are released, the
   claim is dropped, and the gone evidence is written to `lifecycle_evidence`. This is a
   release with verified evidence, and it carries the same guards as the existing
   evidence-gated release in `ordinary_lifecycle::cleanup::complete`, with one deliberate
   difference: the evidence's identity set must equal the binding's recorded identities
   (the empty set for the never-released case), the observation must be fresh within the
   host's `observation_ttl_ms` and not earlier than the step's issued time, so the
   transition takes the ttl, and a replay with identical evidence is accepted as already
   done while different evidence is refused. It does **not** apply `fresh`'s rejection of
   evidence observed after the step deadline: a deadline-triggered failure proves the
   process gone after that deadline by construction, and applying the bound would send
   every timeout failure into the `Uncertain` pause this section exists to remove. The
   coordinator journals the builder's reason with the engine log tail (SPEC §17) after
   the tail has passed the same credential redaction `fingerprint_of` applies to a
   recorded command, extended to blank the engine key value and anything shaped like a
   key or token; the raw log stays in the owner-only log file. It then closes that
   deployment's admission, and the status reads `Closed`. No retry. Other deployments
   are untouched.

   **The way back.** A `Closed` deployment accepts a new Start once its configuration
   is corrected: a new revision starts with a clean slate, and a Stop followed by a
   Start on the same revision is a new generation. §9 proves it live.
4. **Not provable.** `Uncertain` pause with the reservation retained, as today. An
   operator Stop retries the termination.

The retry already built for failures before arm stays: nothing was started, so retrying
is always safe there.

Post-launch retry is deferred. Judging whether a failure is retryable means reading why
an engine died, and an exit on a bad argument, an OOM kill and a port race look alike
from outside. It is designed after S1 to S3 have produced real samples. Likely retryable:
killed by signal, a bind race, a transient CUDA initialization error. Not retryable: a
nonzero exit during argument or model load, a missing executable, a readiness timeout.

ADR 0011 decision 5 row 3 is amended to: a launch that fails after arm is terminated,
proven gone, released with evidence and is terminal for that start; an unprovable state
is `Uncertain`. The paragraph added on 2026-09-17 about explicit Stop is replaced.

## 7. Configuration: engine installation, model store, model source

**Engine installation.** `standalone_config::host_policy` emits the full vLLM launch
settings the strict schema requires:

| Setting | Env | Default |
|---|---|---|
| `executable` | `MLLM_VLLM_BIN` | required |
| `tensor_parallel_size`, `pipeline_parallel_size` | | 1, 1 |
| `enable_sleep_mode` | `MLLM_DEEP_PARK=off` to opt out | `true` |
| `kv_cache_dtype`, `block_size_tokens`, `cpu_offload_bytes` | | `auto`, 16, `0B` |
| `requested_budget.kv_cache_bytes` | `MLLM_KV_CACHE_BYTES` | `16GiB` |
| `requested_budget.gpu_utilization_pct` | | 10 |
| `requested_budget.swap_space_bytes` | | `0B` |
| `args` | `MLLM_ENGINE_ARGS` | `--max-model-len 4096` |
| `build_fingerprint` | `MLLM_ENGINE_FINGERPRINT` | `<bin> --version` captured at boot |

`MLLM_MODEL_PATH`, `MLLM_MODEL_ID`, `MLLM_PORT` and `MLLM_ENGINE_PATH` go away: the model
belongs to the deployment, the served name is the route, the lifecycle leases the port,
and the engine's `bin` directory is derived. `LiveVllmProfile` and the adapter that
`roles.rs` builds and discards are deleted. These variables are the standalone shortcut;
remote hosts carry the same fields in `host.yaml`.

**Model store.** A host declares `model_store: { path }`, per ADR 0008 and SPEC §16.2's
`storage_pools.checkpoints`. Standalone reads `MLLM_MODELS_ROOT`, default `~/models`, and
refuses to boot if it is not a directory.

**Model source.** A deployment's `model` gains `source`, per ADR 0008: `{type: local,
path}` with an absolute path or one relative to the store, `{type: huggingface, repo,
revision}`, or `{type: http, url, sha256}`. A `local` path may be any folder the mllm
user can read; the owner decided against confining it to the model store, which is the
default root for relative paths and the landing place for downloads, not a fence. For
`huggingface` the `revision` is optional and may be a branch or a tag: when S1b first
materializes the source it resolves the reference to the exact commit, records that
commit as the deployment's locked revision and content fingerprint, and every later
start, on any host, uses the locked commit until someone deliberately updates the
deployment. S1 makes room for the locked commit in the stored shape. An `http` source
must use `https`. The schema and validation land in S1 so the
shape stops moving. Only `local` is executable in S1; the others are refused at deploy
with a message saying the source cannot be materialized yet. For `local`, the content
fingerprint is a hash of the directory manifest, every file's relative path, size and
modification time, computed at first deploy: seconds on any checkpoint size, and it
detects a swapped or edited file though not a byte-identical rewrite that preserves
timestamps, which the owner accepted. It replaces the placeholder standalone writes
today. Amended after the S1 review: S1 landed the source shape and the resolution
rules, not the manifest hash, so standalone still writes the placeholder and the
hash lands with materialization in S1b. The status runbook carries this as an open
item. The builder receives a resolved absolute path and
never sees the source. Materialization is slice S1b.

## 8. The Fake engine leaves the product

The owner's rule: the final binary is clean of development and testing artifacts, and
the Fake is a test fixture only. A build switch is not enough; when test targets are
built, Cargo unifies dev-dependency features into the shared build, so a feature-gated
Fake would still end up in the binary that was built beside the tests.

The Fake engine, the fake launcher, their lifecycle simulation and the shared test
fixtures move into a new crate, `crates/mllm-testkit`, which no product crate depends
on. Only test targets pull it in, through dev-dependencies. Product code loses every
Fake item: the `fake` engine family and its launch settings in `mllm-config` and
`mllm-domain`, `AdapterSpec::Fake` and its `resolve` arm, `OwnedCoordinator::spawn_fake`,
the embedded fake host in `mllm-agent`, the embedded fake in the legacy controller, and
the Fake branch in `roles.rs`.

Tests reach the Fake through ordinary injection, not a special mode. The coordinator
exposes its driver factory as a constructor parameter, and standalone takes an engine
provider as a parameter; product passes the real one, tests pass the testkit one. The
provider supplies both the builder and the engine installation the host policy is built
from (executable, build fingerprint, launch settings), so a test boots standalone
without `MLLM_VLLM_BIN` and without executing any engine binary at boot. Test
deployments describe a real engine family and the injected builder ignores the launch
settings. `roles::start_standalone` without a configured engine installation returns
`StartError::NoEngineInstallation` naming the variables it expects.

`ParkPolicy` is defined in the Fake module today but is the vLLM adapter's park gate,
carried by `AdapterSpec::Vllm`, built by `ProfileBindings` and stored by the legacy
controller. It moves to a product module in `mllm-adapters` before the Fake leaves.

Ordering inside S1: the vLLM Initialize and Cleanup path is built first against the
existing Fake fixtures, so the first native-launch evidence arrives as early as possible;
the Fake extraction lands after that and before the live run, because L10 depends on
it. The coordinator suite and the A1 gate keep
running on the Fake through the testkit. They are a pre-check and never count as done.

## 9. Live run on host-a

> Retired 2026-09-23 (owner decision 2026-09-22): the suite and runner below drove
> standalone in process, not the shipped binary. They are deleted, and their scenarios
> are matrix rows driven through the shipped CLI and roles: L1–L5 and L11 are M73, L6–L8
> are M38, L9 is M74 (the readiness bound is now the deployment's `timeouts.initialize`),
> L10 is M75 plus `scripts/check-release-clean.sh` in `scripts/live/matrix/sync.sh build`
> (`docs/superpowers/plans/2026-09-22-two-host-engine-matrix.md`, Tier 8). The text
> below is kept as the design record.

`crates/mllm-cli/tests/live_vllm.rs`, gated on `MLLM_LIVE=1`, release build,
`--test-threads=1`, driving only the product path: `roles::start_standalone`,
`app.deploy`, `request_transition`, the real router listener. `live_spark.rs` is deleted.

| ID | Scenario | Proves |
|---|---|---|
| L1 | Deploy qwen3-4b, Start, wait Ready | a real launch through the coordinator; identity durable before the engine runs; API process and worker both recorded; the five launch settings on the command line |
| L2 | Chat through the router with the API key, plain and streaming | the serving path end to end, through the per-deployment forwarder |
| L3 | Access control | the router refuses a request without the user key; the socket table shows the engine on `127.0.0.1:<leased port>` and nowhere else; a connection to that port through the host's routable address is refused; a direct local request without the engine key is rejected; a direct request to `127.0.0.1:<leased port>/sleep` and `/collective_rpc` without the engine key returns 401 and with the key is answered, proving the mllm guard; the per-deployment forwarder refuses an upstream path outside its chat and models allowlist, driven directly rather than through the router's route table, which has no such route to begin with |
| L4 | Stop | the whole process group empty, workers included; port lease and grant released; `observed_state=stopped` |
| L5 | Start again | a new incarnation, pid and engine key reach Ready (T10) |
| L6 | Model source at an empty directory, Start | the engine exits; mllm proves gone, releases, reads `Closed`; the journal carries the reason and a redacted log tail; no `vllm` process remains |
| L7 | Recover from L6: correct the model source, Start | the deployment reaches Ready and answers a request |
| L8 | An engine executable that exits at once (`/bin/false`), Start | reads `Closed`, not `Uncertain`; no blocked helper process is left |
| L9 | Healthy model, 20 s start deadline | the engine is alive at the deadline; mllm terminates it, proves gone, reads `Closed` |
| L10 | The release binary, built by `cargo build --release --bin mllm` with no test targets | standalone without `MLLM_VLLM_BIN` fails `NoEngineInstallation`; the binary contains no testkit or Fake symbol |
| L11 | Memory | ledger reservation against `/proc/meminfo` before Start, at Ready and after Stop; recorded; asserted only that Stop returns within tolerance of baseline |

`scripts/live/run-on-spark.sh` syncs the working tree to `host-a:~/mllm-f2` and never
touches `~/mllm`, builds the product binary and the test targets as two separate
invocations (`cargo build --release --bin mllm`, then `cargo build --release --tests`,
both with `PROTOC=$HOME/.local/bin/protoc`) so L10 inspects a binary built without test
targets, runs the test with `MLLM_VLLM_BIN=~/mllm-vllm-venv2/bin/vllm` and
`MLLM_MODELS_ROOT=~/models`, and copies logs back under `target/live/<timestamp>/`. The
host name is fixed to `host-a`; any other is refused, and `host-b` is never
contacted. A pre-flight refuses to start when another engine process is already on the
box and prints what it found. Nothing is killed by name.

Evidence goes in one hardware runbook for all slices, `docs/runbooks/spark-live-f2.md`:
commit, vLLM version, model, commands, scenario results, timings, memory samples and
failures. `docs/runbooks/f2-current-status.md` stays the single status authority and
links to it. Raw logs stay out of git.

## 10. Done

S1 is done when L1 to L11 pass on `host-a` and are recorded in the evidence runbook,
the CPU suite and clippy with warnings denied are green, and one review has been
answered. One release path has no live coverage: the no-identity branch of §6, which
releases on the never-released spawn outcome. The product path always records the
gate shell's identity first, so no live scenario can reach it; it is proven by the
failed-association unit test, and that exception is stated here rather than implied.
CPU and Fake tests are a pre-check. They are never the claim.

## 11. Review record, 2026-09-17

Six reviewers read this design (coherence, feasibility, security, scope, adversarial,
product); the blocking claims were checked against the code before acting. The owner
decided each of the 19 findings.

Applied as proposed: the `record_launch` precondition; recording the workers and
requiring an empty group; the Initialize time limit and the grace that must fit;
the no-identity case as its own proof; disposing of a never-released child; the engine
key; the loopback and engine-key checks in L3; the engine log written to a file; the
release guards; the recovery scenario; blocking waits off the async threads; redaction
of the log tail.

Applied in the owner's version: the Fake moves to a test-only crate instead of a build
switch ("the final binary should be clean of development and testing artifacts"); the
five launch settings are implemented, not documented ("actual implementation"); parking
is a requirement of mllm, so its protections are requirements too; restart recovery
re-attaches by recorded pid, start time and boot id, as slice S1r right after S1;
Hugging Face references are fetched and locked to a commit rather than refused.

Settled against the reviewer: a local model source may be any folder ("any folder is
okay"); no confinement check is to be added.

Decided after the round: deep parking is on by default and a host opts out ("It is opt
out of deep parking. We will do it by default"). SPEC §9.1, T21 and `AGENTS.md` are
amended by the S2 ADR; S1 launches with sleep mode and development mode enabled.
Development mode is what makes the sleep routes exist, so the owner ruled it "a
requirement with an opt out"; the unauthenticated control routes found on the box
the same day are therefore guarded by mllm's own middleware from S1 (§3).

Withdrawn: the feature gate missing four crates, since no feature exists any more.

Found while revising, not raised by any reviewer: the router's boot-time forwarding
table cannot reach an engine on a leased port with a per-launch key (§3); and a wrong
engine path does not reach the no-identity branch, because the launcher spawns `sh`
first and the identity recorded is that shell's, so L8 exercises the ordinary
proven-gone path and the no-identity branch is covered by a unit test of a failed
association instead.

Left for later designs: what is
hashed for a large local checkpoint's fingerprint and what it costs (plan); which
`committed_epoch` a failed-launch release writes (plan); SPEC §20 identifiers for L1 to
L11 (plan); a loopback-only port range for leases; `--trust-remote-code` on the approved
argument list, which predates this design.

### Round 2

Six reviewers re-read the revision with the round-1 decisions in hand. No rejected
finding was re-raised; every round-1 fix was verified as landed. Two counts were fixed
silently. Twelve findings were routed by the owner to best-judgment resolution; eleven
were applied: the failed-launch release drops the step-deadline freshness bound; the
engine key never reaches argv and the builder assembles the environment; the store
rule, not the domain rule, is what counts identities to two; the cipher is named
(XChaCha20-Poly1305, associated data, deletion on release); `security.deep_park:
disabled` also turns sleep mode and development mode off; L3 tests the engine's control
routes directly; `LifecyclePort` gains the endpoint-and-key projection; the injected
provider supplies the engine installation; `observe_group` may return empty and the
group observer is extended; `ParkPolicy` moves to a product module; the builder's waits
end 2 s before the deadline. The twelfth, the legacy controller, the owner decided:
retired inside S5, closing SPEC §18's parallel-controller clause.

The owner also decided the items previously left for later: the local fingerprint is a
manifest hash; S1b moves after S5; `--trust-remote-code` becomes a host opt-in; leased
ports stay in the general range with `--host 127.0.0.1` as the control and L3 as the
proof. Of the ten FYI observations, the author adopted: S1r proves socket ownership
before sending the key; a key that fails to decrypt is unprovable; the SGLang key table
is installation-scoped and separate; the two "deep park" switches are one; requested
budget may not exceed the Ready allocation; the Fake extraction follows the first
native-launch evidence; L3 drives the forwarder allowlist directly; the launcher's
existing escalation and group observer are reused. Not adopted: none.
