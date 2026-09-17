# Native launch through the coordinator, vLLM first

**Date:** 2026-09-17
**Status:** Approved by the owner in design review; awaiting the implementation plan.
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
parked/active and parked/parked, live on `host-a`. That is five slices, each with its
own design, plan, live run and review:

| Slice | Deliverable | Live proof |
|---|---|---|
| **S1** | vLLM launches, serves, stops and fails through the coordinator | restart-only cycle and two failure cases |
| **S2** | Ordinary park and wake, vLLM, wired to `mllm-domain/src/park.rs` | active to parked to active, serving after wake |
| **S3** | SGLang launches through the same mechanism | restart-only cycle on SGLang |
| **S1b** | Model source materialization (`huggingface`, `http`) into the host model store | deploy from a repo reference |
| **S4** | Park and wake, SGLang | active to parked to active |
| **S5** | Eviction in the authority, coexistence, the scenario matrix | the four residency pairs and pressure-driven switching |

Merges to `main` happen after S1 and S3 are both live-green, and again after S5.

Decisions taken in this review that bind later slices:

- **SGLang keys (S3).** The admin and inference keys belong to the engine installation
  (ADR 0008). mllm generates them when an installation is registered, stores them in
  SQLite encrypted, and passes them to the process at launch through the private
  descriptor the wrapper already reads. The encryption key lives in
  `<state_dir>/identity/`, owner-only, generated at first boot, never in the database.
  `credential_ref` on a profile becomes a real reference into that table.
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
    /// SIGTERM the owned group, wait `grace`, SIGKILL, then prove every identity gone.
    fn terminate_owned(&self, identities: &[ProcessIdentity], grace: Duration)
        -> Result<(), RuntimeError>;
}
```

`Presence` moves from `mllm-launchers/src/process_absence.rs` to
`mllm-domain/src/completion.rs`, beside `ProcessIdentity`, so both crates can name it.
The older `Launcher` trait and `OwnedHandle` stay for the legacy F1 Controller; nothing
new uses them.

`crates/mllm-launchers/src/owned_launch.rs` implements it as
`DurableProcessLaunch { spawn: DurableSpawn, association: Arc<dyn LaunchAssociation> }`.
`spawn_durable` calls `spawn_persisted`; an outcome with
`initialization_acknowledged: false` is an error carrying the reason, and the gated child
is never released. `terminate_owned` is new and is specified in §5.

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
| `sleep_flags` | only when `enable_sleep_mode && security.experimental_controls`; empty in S1 |
| `api_key` | `None`; the engine listener is local and unauthenticated, as in F1 |
| `engine_log` | `<state_dir>/logs/<deployment>/<incarnation>.log` |

`render_command` in `crates/mllm-adapters/src/vllm/args.rs` gains the two flags mllm
reserves and does not yet emit: `--host 127.0.0.1` always, and `--served-model-name` from
the new `PlanInputVllm.served_model_name`. Profile arguments render after them and are
validated by `validate_profile_args` as today.

Unchanged: the `EngineAdapter::execute_persisted` signature, the ordering of the worker's
`drive`, the store schema up to §6's one new transition.

## 4. The vLLM Initialize step

`VllmAdapter::execute_persisted` for `RuntimeAction::Initialize`, with context
`identities: OwnedLaunch`, a Ready completion target and vLLM launch settings. Every wait
is bounded by `context.deadline_ms`.

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
6. **Observe.** Return `EffectObservation` with `identities: vec![identity]`, a receipt
   naming the fingerprint and endpoint, and the facts a cold start proves:
   `AllocationsRestored`, `WeightsUsable`, `CacheValid`, `ModelUsable`.

The coordinator then records the owned launch, whose first identity matches the one
already stored by the association, and completes the step. The retained `Driver` keeps
the adapter for Cleanup.

Park effects return `Unsupported` from the vLLM builder until S2. vLLM's `EngineCore`
children are covered by process-group termination and are not recorded individually.

## 5. Termination and cleanup

Today every resolved adapter's cleanup closure is `observed_gone`: read `/proc`, report.
For native builders the closure becomes:

1. `tools.terminate_owned(&context.identities, grace)`. For each identity, check presence
   first. Already gone needs no signal. Present with matching boot id and start ticks is
   signalled. The same pid with a different start identity is refused as `Uncertain`; mllm
   never signals a process it cannot prove it owns (SPEC §13.2). `SIGTERM` goes to the
   process group of the API process, which `DurableSpawn` made a group leader. Poll
   `verify_gone` every 200 ms until `grace`; then `SIGKILL` and poll for a fixed 5 s.
2. `observed_gone(&context, &clock)`. `AllGone` yields `CleanupEvidence`. `SomeAlive` or
   `Indeterminate` is an error; ownership and the reservation are retained.

`CoordinatorOptions.terminate_grace` defaults to 15 s and is validated between 1 s and
300 s. The whole closure stays inside the cleanup deadline the store already bounds.

`CleanupMode::TerminateOwned` is the closure above. `InspectOwnedGone` is `observed_gone`
alone. Both take identities from the context, so they do not depend on adapter memory.

Out of S1: recovering a running engine after a coordinator restart. `drive_cleanup`
refuses a binding the worker does not retain. That is SPEC §13.2 reconciliation and is
recorded in the runbook.

## 6. Launch failure in v1: terminate, prove, release, fail

`RuntimeError` has no failed variant, and once a step is armed the worker turns any error
into `Uncertain` and pauses until an operator sends Stop. A vLLM that dies on a bad model
path would wait for a human. The coordinator instead classifies by proof, not by the
builder's opinion. After any Initialize error on a native builder:

1. Read the binding's recorded identities from the store. None recorded means the gate
   never opened; the child is a blocked shell that exits when the tool drops its pipe, and
   no engine ran.
2. `terminate_owned`, then `verify_gone`.
3. **Proven gone.** New store transition
   `release_failed_launch(session, step_id, evidence, now)`, one transaction: the step
   becomes `cancelled` and its lifecycle run and operation `failed` (the schema's step
   states have no failed value and its run states do, so no migration is needed), the
   binding becomes `released`, the endpoint lease and resource grant are released, the
   claim is dropped, and the gone evidence is written to `lifecycle_evidence`. This is a
   release with verified evidence, so the working agreement's invariant holds. The coordinator journals
   the builder's reason with the engine log tail (SPEC §17), closes that deployment's
   admission, and the status reads `Closed`. No retry. Other deployments are untouched.
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
| `enable_sleep_mode` | | `false` in S1 |
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
revision}`, or `{type: http, url, sha256}`. The schema and validation land in S1 so the
shape stops moving. Only `local` is executable in S1; the others are refused at deploy
with a message saying the source cannot be materialized yet. For `local`, the content
fingerprint is computed from the directory manifest at first deploy instead of the
placeholder standalone writes today. The builder receives a resolved absolute path and
never sees the source. Materialization is slice S1b.

## 8. The Fake engine is test-only

The owner's rule: the Fake is acceptable for tests and must not be in a release.
`mllm-adapters` gains a cargo feature `fake-engine`, off by default. `FakeEngine`,
`AdapterSpec::Fake`, the Fake arm of `resolve` and `Engine::Fake` handling in
`ProfileBindings` compile only with it. Test targets enable it through dev-dependencies.
A release build contains no Fake symbol. `roles::start_standalone` without a configured
engine installation returns `StartError::NoEngineInstallation` naming the variables it
expects. The coordinator suite and the A1 gate keep running on the Fake under the
feature. They are a pre-check and never count as done.

## 9. Live run on host-a

`crates/mllm-cli/tests/live_vllm.rs`, gated on `MLLM_LIVE=1`, release build,
`--test-threads=1`, driving only the product path: `roles::start_standalone`,
`app.deploy`, `request_transition`, the real router listener. `live_spark.rs` is deleted.

| ID | Scenario | Proves |
|---|---|---|
| L1 | Deploy qwen3-4b, Start, wait Ready | a real launch through the coordinator; identity durable before the engine runs |
| L2 | Chat through the router with the API key, plain and streaming | the serving path end to end |
| L3 | The same request without the key | the router refuses; the engine listener is bound to 127.0.0.1 |
| L4 | Stop | the whole group gone including `EngineCore` children; port lease and grant released; `observed_state=stopped` |
| L5 | Start again | a new incarnation and pid reach Ready (T10) |
| L6 | Model source at an empty directory, Start | the engine exits; mllm proves gone, releases, reads `Closed`; the journal carries the reason and log tail; no `vllm` process remains |
| L7 | Healthy model, 20 s start deadline | the engine is alive at the deadline; mllm terminates it, proves gone, reads `Closed` |
| L8 | The release binary | standalone without `MLLM_VLLM_BIN` fails `NoEngineInstallation`; no `FakeEngine` symbol in `target/release/mllm` |
| L9 | Memory | ledger reservation against `/proc/meminfo` before Start, at Ready and after Stop; recorded; asserted only that Stop returns within tolerance of baseline |

`scripts/live/run-on-spark.sh` syncs the working tree to `host-a:~/mllm-f2` and never
touches `~/mllm`, builds with `PROTOC=$HOME/.local/bin/protoc cargo build --release
--tests`, runs the test with `MLLM_VLLM_BIN=~/mllm-vllm-venv2/bin/vllm` and
`MLLM_MODELS_ROOT=~/models`, and copies logs back under `target/live/<timestamp>/`. The
host name is fixed to `host-a`; any other is refused, and `host-b` is never
contacted. A pre-flight refuses to start when another engine process is already on the
box and prints what it found. Nothing is killed by name.

Evidence goes in one hardware runbook for all slices, `docs/runbooks/spark-live-f2.md`:
commit, vLLM version, model, commands, scenario results, timings, memory samples and
failures. `docs/runbooks/f2-current-status.md` stays the single status authority and
links to it. Raw logs stay out of git.

## 10. Done

S1 is done when L1 to L9 pass on `host-a` and are recorded in the evidence runbook,
the CPU suite and clippy with warnings denied are green, and one review has been
answered. CPU and Fake tests are a pre-check. They are never the claim.
