# SGLang adapter qualification readiness

Status: deterministic adapter slices pass; native startup and F2 qualification
remain incomplete. No native launch or live inference was performed for this
readiness checkpoint. No owner decision is currently required.

## Implemented and verified

| Boundary | Implementation evidence | What it does not establish |
| --- | --- | --- |
| Protected candidate handoff | `49bed60`; consolidated handoff review cleared Critical/Important findings | Native ServerArgs mapping or complete process enrollment |
| Reviewed placement | `0706d3f`, `bf4b209`; frozen selection and read-only physical UUID/PCI corroboration | Trusted device-policy provisioning or guarded CUDA namespace composition |
| Checkpoint, selected source preflight and child guards | 212 committed runtime tests, including protected scheduler listener and pre-unpickle child guards; twelve startup-guard tests also pass in the isolated Spark environment | Installed binary compatibility, complete native startup or whole-package attestation |
| Process-group observation | `67299e3`; 31 launcher tests pass | Native role enrollment, no-escape contract, or cleanup authority |
| Typed controls | `6c78908`; 17 deterministic control tests | Real saver behavior, durable coordinator composition, qualified memory release |
| Shared asynchronous forwarding | `d335d37`; 13 cross-engine contract tests, all 95 adapter and 25 router tests pass | Durable request settlement, global buffer admission, or live output correctness |
| Owned controller state | `c551e43`; lock-before-session startup and seven focused tests | Production worker/API cutover or restart reconciliation |
| Durable snapshot | `359a827`; seven snapshot and ten event tests | Full public snapshot semantics, live observations, or complete writer event coverage |
| Management authentication and SSE | `b2db142`, `6f8baea`, `dcb3265`, `5be054e`, `3f24b70`; 28 tests cover protected credentials, bounded authenticated snapshots/events, replay and all five qualified Initialize projections | Listener/CLI composition or complete public projection |
| Scheduler observation connection | `afe5b2b`, `4f9f180`; bounded Rust client, protected Python listener, actual cross-language transport test | Scheduler startup attachment, native allocation truth, or qualification |
| Stopped managed configuration | `06dbb78`, `c2d3a7b`; authenticated create/replace commands share owned Store/session; all 38 management tests and all-target Clippy pass | Activation, lifecycle actions or complete public command composition |
| Current policy and historical retries | `294bfe3`, `c2d3a7b`; stopped HTTP commands compose current controls and preserve accepted historical retries | Lifecycle command composition or retroactive changes to old receipt formats |
| Ordinary Qualified Fake initialization | `3f24b70`; atomic acceptance/arm/owned association/Ready, uncertainty retention and scoped provenance reuse; 551 combined tests and five-crate all-target Clippy pass | Owned worker completion, warm lifecycle, ordinary cleanup, recovery or native qualification |

Adapter all-target Clippy passes with warnings denied. These are CPU/fake-server
results. They are not Q11 evidence and never set Qualified.

The selected recipe is Qwen3-4B-Instruct-2507 at checkpoint revision
`cdbee75f17c01a7cc42f958dc650907174af0554`, using SGLang v0.5.16 source
`fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`. TP=1, DP=1, one tokenizer worker,
BF16, context/token pool 4096, maximum concurrent requests 8, prefill/decode
graphs disabled, real memory saver required, CPU weight backup disabled, and
disk reload on restoration. Additional feature combinations remain unqualified.

## Remaining implementation gates

- Complete the effective pinned ServerArgs map and startup checks. Corroborate
  physical placement, prohibit unreviewed plugins, protect startup logging, and
  verify the real saver instead of trusting an enable flag.
- Bind the installed protected wrapper/helpers to the reviewed runtime recipe.
  Wire the production clock, credential provider, checkpoint/source revalidation, and
  complete API/scheduler/detokenizer enrollment. Existing two-role Fake fixtures
  do not establish the native process topology.
- Connect fresh native observations to each separately persisted drain, release,
  resume, reload, flush, and probe intent. A lost response remains uncertain;
  retain peak accounting and never replay a possibly applied control.
- Complete the A2d/A3 coordinator, management API, CLI, inventory, and routing
  cutover. Delivery now awaits bounded queue capacity. Failed or timed-out delivery
  cannot append a successful terminal, while backend parsing continues within its
  independent bounds. Uncertain streaming/nonstreaming work retains its process-local
  charge. Durable settlement/reconciliation and global buffer admission remain open.
  Neither transport closure nor client disconnect settles backend work.
- Extend trusted qualification collection/catalog beyond its current Fake
  program. Qualification requires exact recipe, checkpoint, source, saver,
  Torch/CUDA, hardware/environment, binding, and process identities.
- Implement and test the F2C API-based runner before using it for live cases.

The current native entrypoint remains closed. A passing source preflight or
adapter construction cannot remove any of these gates. Ordinary dispatch stays
closed for candidate bindings; successful candidate work retains conservative
accounting until the appropriate verified transition commits.

New configuration receipts retain command identity separately from resolved host
policy. Unchanged retries use their original resolved deadline even after policy
or profile changes. New submissions still require current persisted controls.
Pre-integration V1 receipts retain their original full-resolution rule rather
than being rewritten with guessed command identity.

## Sole live execution and evidence owner

[F2C](../superpowers/plans/2026-09-12-f2c-mixed-engine-qualification.md) owns live
execution and versioned artifacts under `.context/f2-qualification/`. This
runbook consumes its final case IDs/evidence references; it does not rerun cases
or copy raw artifacts. There are no passing native evidence references yet.

- F2C Task 3: read-only preflight, then each engine independently: one cold
  initialization, 32 nonstreaming and 32 streaming requests, five park/wake
  cycles with the corpus after every wake, followed by verified cleanup. Test
  authentication and private health-generation protection. Failed runs do not
  promote; ordinary bindings require verified candidate cleanup first.
- F2C Task 4: prepare and retain both runtimes; prove same-engine and mixed-engine
  isolation/coexistence; run ten A→B→A cycles with 320 total post-wake requests;
  prove joined activation, bounded queues, fairness, and impossible-arrangement
  refusal without cold-stop fallback.
- F2C Task 5: three repetitions of each selected lost-reply/restart scenario,
  API/CLI operations and reconnect behavior, then Q1–Q11 evidence closeout with
  counts, failures, timeouts, and direct/routed cold/warm timing distributions.

Only host-a is authorized. Existing authorization covers its isolated SGLang
environment and reviewed saver-observation patch; it does not cover driver
changes, existing engine environments, reboots, other hosts, or broad cleanup.
The patch is implemented locally but is not yet installed in that environment.

F2C guardrails remain authoritative: protected available memory is
`max(16 GiB, ceil(MemTotal/5))`; initial single-engine grants are 48 GiB per phase,
not proven memory limits. Use 250 ms observations no older than 2 s, a stable
30-second swap baseline, and stop new admissions on pressure or the prescribed
swap-growth threshold. Do not derive per-owner release from global free memory.
Cleanup requires frozen run permission and verified owned identities.

The deterministic `tests/harness/src/f2_pressure.rs` guard now implements the
fixed headroom/managed bounds, 30-second stable baseline (including existing swap),
64-MiB/three-increase abort rules, and a no-I/O freshness watchdog. Errors latch
for the run. Eight pressure tests and all thirteen harness tests pass, with
all-target Clippy warnings denied. The runner must still wire 250-ms collection,
watchdog scheduling, and admission closure; the guard performs no engine effects
and does not establish host identity, attribution, or qualification.

## Owner-attention tracking

No pending owner choice at this checkpoint. If qualification requires a new
engine/source recipe, driver/environment change outside existing approval,
unavailable credentials, or expanded cleanup authority, record the exact blocker
and request direction. Implementation dependencies are not owner blockers.

F2 remains open until both selected warm recipes and all required automated/live
gates pass. No wake-time or throughput improvement is assumed.
