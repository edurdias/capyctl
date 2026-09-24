# F2 continuation status

F2 is not complete. Work continues on `feat/f2-sglang`; no push or final merge is
claimed. The current user instruction is one consolidated review at the end,
not per task. Focused TDD and integration verification continue throughout.

## Consolidated review round — 2026-09-24 (uncommitted)

The owner's single end-of-work review ran as four read-only reviewers (store;
controller and scheduler; agent, protocol, adapters and runtime with a security
focus; router, management, CLI, config and test hygiene), followed by fix agents
per area. Security: the sensitive-option gate now decides on the destination the
engine's own parser resolves (closing bind and path bypasses such as SGLang
`--decoupled-spec-bind` and vLLM `--master-ad`), code-loading options need host
approval, vLLM control traffic uses a separate admin key with no proxy or redirects
(remote and embedded), engines start from a closed environment with plugins off,
bytecode is neither written nor loaded, the whole runtime tree is integrity-checked,
chat bodies are allowlisted while tool calls, structured output, reasoning and
multimodal content still pass (SPEC §10), and secrets are redacted from debug
output. Controller: an unproven cleanup pauses only its own binding and retries when
the host returns instead of halting every lane; leaked lease grants close; uncertain
leases no longer block switching; stale results no longer end host sessions; pauses
apply immediately. Store: one instance's failure closes only that instance; parks
and restores never reopen a gate another reason closed; deferred stops stop ready
siblings at once; embedded cleanup never releases on empty evidence; switch
closures are cleared; checkpoint digests are accepted only from resolved hosts.
Router and management: uncertain ends no longer leak in-memory slots; waiting
requests are served first-in first-out within their deadline; the configured body
bound applies; the events stream projects every event kind; replays cannot undo an
operator stop; `start --evict` validates before evicting; a request deadline under
30 s is refused. Drains record their intent before any stop (store schema v30), and
a legacy generated `standalone.yaml` with the old `tls` block starts with a warning
instead of being refused. Test roles can no longer be orphaned and fixed-port
collisions are gone. Latest local verification: core 972, workspace 1671 and
Clippy pass on CPU and fake engines; not yet re-run live.

## Embedded vLLM separate admin key — 2026-09-24 (uncommitted)

SPEC §9.1 / T21, ADR 0012. Embedded (standalone) vLLM now uses a separate admin key,
as the remote path already does. `ProfileBindings` issues a fresh admin key beside the
inference key (`AdapterSpec::Vllm.admin_key`). The resolved-spawn factory seals both
roles before the builder runs and refuses the launch if the two keys are equal. The
engine gets it as `MLLM_VLLM_ADMIN_KEY`. `runtime/mllm_vllm_guard.py` then admits the
inference key only on `/v1`-family paths and `/metrics`. The adapter presents the admin
key on `/sleep`, `/wake_up`, `/is_sleeping`, `/collective_rpc` and `/reset_prefix_cache`.
Ingress and the router still read only the inference role. The standalone host policy
now names `admin_credential_ref: secret://admin-key` for vLLM too. This changes the
standalone vLLM recipe fingerprint.

Migration: a launch recorded before this change sealed one key. A restarted coordinator
adopts it with that key only (`local_adoption`) and never mints an admin key for a
running engine. That engine keeps the single-key guard until it next launches. Every
new launch seals both roles.

Tests (T21 T37): the spec carries distinct fresh keys, an embedded start seals both
roles and the runtime endpoint carries only the inference key, adoption with and without
a recorded admin key, and HTTP key routing against a keyed-guard mock (with and without
an admin key). The standalone profile names both references. Core, workspace and
clippy logs are `target/orch-logs/key-*.log`. These are CPU and Fake-engine tests only,
not native qualification: no Spark run has exercised the two-key embedded guard.

## Status reasons, solo-first-start switching and launch-failure reasons — 2026-09-24 (uncommitted)

Three local fixes from the M53/M53D/M66 live findings. They were not run on the Sparks.

1. SPEC §6.4. Status now shows `latest_operation {id, kind, state, error_code, reason, hint}`
   for each deployment and each instance, and `error_code` on each `operations[]` entry.
   All of these fields are additive. The reason is the latest journal evidence for an
   operation that did not succeed. It is cut to one line of at most 512 bytes, with no
   engine log tail, and it is withheld if it might quote a credential
   (`mllm_domain::diagnostics`). An error code is shown only when it is a closed code.
   The hint is fixed text for the closed category. `deploy --wait`, and the new
   `start deployment|instance --wait`, print the reason and hint when they fail.
2. When a switch target needs a solo first start (a whole-host startup footprint),
   the plan now releases every other charge on the host up front. Those victims stop
   instead of parking (`accept_switch_release(.., may_park)`). Live M53 showed the old
   order: the victim parked, the first arm failed on insufficient resources, the start
   sat out a 30 s retry cooldown, and only then was the parked residual reclaimed.
3. An engine that exits before readiness is now a launch failure, not ownership
   uncertainty. The host sends `MemberExecutionResult.launch_failure`, a new proto field
   (13). It is one printable line of at most 256 bytes: the exit code or signal, and the
   names of any options the engine refused, never their values. The controller maps a
   result that reports launched, not usable, all processes gone to
   `RuntimeError::LaunchFailed`. The launch is still released only on the host's
   verified gone evidence.

Harness: `rows/M53.sh` (the recheck extension), `M53D.sh`, `M66.sh` (a long count prompt,
checked with `residue_check` after the delete) and `SGLMO.sh` come from the session
scratchpad. `run_row.sh` gains `SCRATCH_ROWS`. `residue_check` ignores the deleted id's
tombstone; the earlier inline check wrongly failed M53D and M66 on it.
The core, workspace and clippy logs are `target/orch-logs/fix8-*.log`.
These are CPU and Fake-engine tests only, not native qualification.

## SGLang 0.5.20 standalone product gate passed — 2026-09-21

The final objective is a full multi-node test: control-host controls host-a and
host-b. Standalone on host-a is the first gate, not completion. The
server/agent transport and distributed group support must be verified before
claiming multi-node success; independent SSH launches do not meet that goal.

The owner explicitly authorized a clean SGLang 0.5.20 installation on host-a,
then required fixes in product code and validation through the shipped CLI.
This authorization permits the SGLang environment migration; drivers, reboots,
and unrelated environments remain out of scope.

The clean private environment is `~/mllm-sglang-0.5.20-venv`. Its installed
SGLang source matches release commit `94602c9c2b7cbdb8efd5c52802dac6a1c180089e`;
the runtime now checks 86 source files, including the new argument groups.
CUDA allocation passed with PyTorch 2.13.0 / CUDA 13.0. The package checker
reports a cuSPARSELt wheel metadata incompatibility (`manylinux2014_sbsa`
inside the aarch64 wheel); its ELF architecture is AArch64. No installed
package code or metadata was patched. Old helper directories were moved to
`~/mllm-archive/sglang-before-0.5.20/`; the old environment is now archived there too. Restore it to its original
`~/mllm-sglang-f2-venv` path before attempting rollback.

Uncommitted product fixes adapt ServerArgs resolution to 0.5.20, share the
LaunchSpec type across script/import execution, publish the observed host name,
and preserve physical GPU UUIDs when applying persisted resource controls.
The owner requested an explicit `start standalone --debug-engine-logs` flag;
it retains full private native logs, may include secrets, and keeps raw logs
out of management errors. Without that flag raw native output is suppressed.
The UUID regression reproduced loss before the fix and passes afterward.
The CLI now connects deploy/start/stop/status to the authenticated management
API, with a separate loopback listener and admin credential. A binary-level
Fake-engine test passes; that is not native qualification.

Live validation uses `target/release/mllm start standalone` and
`mllm deploy model --file ... --activate --wait` on host-a. Evidence and
private application state are under `~/mllm-runs/sglang-0.5.20-product/`.
The first product deployment exposed lost GPU identity and failed closed at
placement. The second (`qwen3-4b-v2`) passed placement and loaded weights, but its first
inference failed because the guarded environment omitted the venv tool path:
FlashInfer could not execute `ninja`. The launcher now builds PATH from the
selected interpreter's bin directory plus fixed system directories. It never
inherits the caller's shell PATH. Explicit retry of a verified-clean failed
launch is also fixed; it creates fresh operation and binding identities while
preserving every retained-state guard. The corrected product passed twice:
with debug logging and with the default suppressed-output mode. Each run reached
Ready with dispatch enabled and answered authenticated inference through
`127.0.0.1:8443` with HTTP 200 (`2 + 2 → 4`, then `3 + 4 → 7`).
Each CLI stop settled stopped with admission and dispatch disabled. Both owned
process groups disappeared and nvidia-smi showed no compute processes.
After the default-mode stop, MemAvailable was 123,863,424 kB.
The final deployment generation is 3; no park/wake or multi-node success is claimed.
SQLite was queried read-only for diagnosis; no state rows were edited.

Checklist:
- [x] Clean 0.5.20 environment; preserve rollback and previous failure evidence.
- [x] Native Ready, authenticated routed inference, CLI stop, owned-group cleanup.
- [x] Default-off full debug log flag, exercised in both modes through the binary.
- [x] Complete final Rust integration check (635 distinct core tests).
- [ ] Complete consolidated code review.
- [ ] Implement server/agent enrollment, transport, reconciliation and remote lifecycle.
- [ ] Implement and validate distributed group launch/accounting on both Sparks.
- [ ] Pass the full multi-node gate; standalone success is only its prerequisite.

Non-secret live evidence is copied to `target/live/sglang-0.5.20-product/`.
Full debug logs remain in private files on host-a and were not copied into
the repository or management journal.
Core verification after the retry fix passes 635 distinct tests (636 reported,
including the owned-state child summary). A subsequent tool-path change passes
the full SGLang Initialize target (10 tests). Core Clippy and targeted CLI Clippy
pass with warnings denied. The complete runtime suite now passes 259/259,
including the additional debug argument mapping check. Its pinned saver source
fixture is supplied through `TMS_SOURCE_ARCHIVE`; these are CPU checks.
The native start/inference/stop gate passed. This does not qualify deep park,
wake, switching, or distributed operation.

Read-only multi-node preparation confirms both Sparks are reachable and use
aarch64 GB10 / driver 580.173.02. host-b has the required checkpoint but no
SGLang environment was listed. Product `start server`, `start host`, enrollment and authenticated AgentControl
sessions now pass local binary tests. Remote engine execution and native
two-Spark validation remain pending.

Implementation follows
`docs/superpowers/plans/2026-09-21-1831-feat-two-spark-sglang-plan.md`.
Host-scoped ownership, typed command contracts and the additive namespace
migration are implemented. U1 focused domain/protocol/store checks pass 177
tests; the integrated core run passes 639 distinct tests (640 reported,
including the owned-state child summary), and core Clippy passes with warnings
denied. These CPU/Fake checks do not qualify native multi-node operation.
U2 enrollment, U3 durable execution and U4 remote roles/sessions are implemented.
U5 remote lifecycle and private ingress are in progress, followed by group launch.
The owner reaffirmed on 2026-09-21 that the acceptance target is the full two-node
run, not standalone. The registry-backed configuration test now verifies host
selection, disjoint resource identities, durable original configuration, and exact
request replay. Private ingress tests cover generation fencing, forwarded header
restrictions, streaming request accounting, and persistent separate credentials.
A frozen ingress binding rejects endpoint reassignment and missing remote authority.
The integrated core run now passes 651 distinct tests (652 reported, excluding
the nested owned-state summary); core, agent, config and CLI Clippy pass with
warnings denied. These are local tests, not native qualification. The owner's
Tailscale SSH reauthentication completed on 2026-09-22; read-only SSH checks then
succeeded on both Sparks with no GPU compute process on either. The native remote
gate on host-a is therefore unblocked but not yet run: no source sync, Spark build,
server/host deployment or remote native launch has happened since. host-b
had no known matching SGLang 0.5.20 environment; on 2026-09-22 the owner granted
a scoped exception to create a clean SGLang 0.5.20 virtual environment on host-b
mirroring host-a (same source commit, same wheels), with no driver, system package or
reboot changes and existing vLLM environments left untouched. The owner also
directed that the old host-a standalone service, if still alive, be stopped through
the shipped CLI and that work proceed until blocked or a live milestone is proven.
Later on 2026-09-22 both Sparks were synchronized to the current worktree and built
`target/release/mllm` under `~/mllm-f2` (builds over non-interactive SSH need
`~/.local/bin` on PATH for `protoc`). The host-b environment now matches host-a
byte-for-byte (206 PyPI wheels, identical RECORD digests, same `uv pip check`
cuSPARSELt metadata complaint); an import and CUDA smoke passed without loading a
model, which is parity, not qualification. The old host-a standalone service (PID
158346, ports 7443/8443) had no running engine; the CLI has no `stop standalone`
verb and the standalone role installs no SIGTERM handler, so it was ended with
SIGTERM. That missing graceful stop is an open product gap. Its state directory
still records deployment `qwen3-4b` with desired `ready` and observed `stopped`.

Owner direction on 2026-09-22 changes the sequence. Multi-node work proceeds first
with single-rank recipes: one control-host control plane managing both Sparks, each host
running multiple vLLM and SGLang single-rank deployments, serving, switching models
and parking as needed. The end goal is a two-host control plane supporting both
engines in all meaningful permutations, mapped by an explicit test matrix. Two-rank
(TP2) group work, previously U6/U7, is deferred until after that matrix passes; its
open design questions (residency, peer exposure, NCCL transport, rank readiness,
compensation, owner granularity, placement shape, rendezvous ports) are parked.
The test matrix is `docs/superpowers/plans/2026-09-22-two-host-engine-matrix.md`
(scenarios M01–M72, gaps G01–G17 plus U5-G1…G4, decisions D1–D11) and the work
plan is `docs/superpowers/plans/2026-09-22-two-host-control-plane-plan.md` (units
W0–W13 in waves). Decision E1 (below) supersedes the plan's per-model recipe
approach for G17. Owner decisions so far:
D1 deep parking is enabled by default and a host opts out (the SPEC §9.1/T21
text that says opt-in is to be amended to match); D2 remote vLLM is built now, in
parallel; D3 park/wake is built once through the coordinator with remote
Park/Restore actions; D4 automatic request-driven switching is required now;
D5 the model set is qwen3.8-27b (NVFP4 build as the catalog anchor), qwen3-30b-a3b,
qwen3-14b and qwen3-4b-instruct, mirrored from host-a to host-b with SHA-256
verification and no internet download; D6 fault injection may use signals on
mllm-owned processes and one bounded external memory allocation, never firewall,
interface or reboot changes; D7 "host restart" means restarting the host agent
process, and reboot recovery stays untested; D8 a server restart must fully
re-attach live remote engines through fresh probes, keeping failures charged and
closed; D9 one route may be served by replicas on both hosts, with the router
load-balancing on its own in-flight counts combined with engine metrics that
each host agent scrapes on loopback and reports, and failing over on host loss;
D10 the normal per-host budget is an 80% managed limit with a 10% free reserve,
and a tight budget admits exactly one of the two largest models; D11 undeploy and
graceful role shutdown (server, host and standalone SIGTERM handling) are built
in this phase. The request path stays layered as the owner stated it: controller
(router) to host agent to engines; clients never reach an agent or engine directly.
E1 (same day): every engine must be able to serve any model. The host declares its
engine runtimes; the deployment chooses the model, the runtime and its parameters.
Deployments carry typed common parameters (dtype, quantization, KV-cache dtype,
context, concurrency and similar) plus ordinary engine arguments that pass through
per SPEC §8.2 and §13.3 operator policy, behind an explicit flag that accepts extra
parameters. Settings mllm owns (device, ports, bind addresses, memory grants, keys,
ranks) are always reserved and can never be overridden. Security-sensitive options
(remote code, plugin or code paths, extra listeners) need host-policy approval.
Checkpoint identity is a digest recorded at deploy time and re-verified at launch,
replacing pre-pinned per-model hashes. The single-checkpoint SGLang recipe pin and
the narrow vLLM flag list are over-restrictions to remove. This matches accepted
ADR 0008 (a deployment owns its `engine_config` from its engine family's schema)
and ADR 0011 (mllm validates a recipe's shape and capacity; whether it works is the
user's responsibility).

P1 (same day, after reviewing SPEC §2, §3, §6, §10 and ADR 0008 with the owner):
load-balanced replicas live inside one deployment. A deployment declares a count
of instances (one instance is one engine group) plus optional placement constraints
(allowed hosts or selector, spread or pack, maximum per host); instances may share
a host when capacity allows. The server's scheduler places instances from live
capacity and reservations at activation, records each placement durably, and the
router balances across that deployment's ready instances. Pinning an instance to a
host remains possible through the selector; changing the count is a revision of the
deployment. This replaces the plan's separate replica-route design (W7/W9).
P2: each deployment declares its per-instance memory request; when omitted, mllm
derives it from checkpoint size, requested KV and a per-engine overhead margin.
The first live run of each model measures actual peak use, and matrix budgets are
recomputed from those measurements. Reservations always use the declared or
derived request. P3: stopping or signalling any role, standalone included, is a
service restart: admission closes, in-flight streams finish or cancel within a
bound, engines stay running and owned, and the next start re-attaches them through
the fresh-probe path. A separate explicit drain (`drain host`, `stop standalone
--drain`) stops every engine with verified cleanup and leaves deployments eligible
for on-demand activation. P4: vLLM development-mode exposure under default-on deep
parking is accepted for this phase with the mandatory mitigations, provided
status and inspect mark every deployment and host profile that exposes those
controls, and park rows run only after the security row (M08) passes live.

Designs: ADR 0013 (deployment instances and placement) and ADR 0014 (deployment
engine configuration, E1 and P2) are written; the plan is revised into waves.
Owner answers to their open questions on 2026-09-22: Q5 on-demand activation
starts one instance, and the rest start only where they fit without eviction,
while explicit `start deployment` brings up all instances; Q6 mixed-engine
load balancing is tested as two deployments on two routes, since one deployment
names one engine installation; Q7 per-instance `stop instance` and `start
instance` verbs are added now (this amends ADR 0013, which proposed none); Q8 a
non-count revision stops all instances and restarts on the new revision, with
rolling replacement designed later; Q9 the checkpoint is fully hashed on first
placement on a host and whenever any file's size, mtime or inode changes, and each
launch or wake re-checks that metadata and rehashes the small files; Q10 ordinary
extra engine arguments are allowed unless the host denies them, and
security-sensitive options always need named host approval; Q11 vLLM launches
through a new `runtime/vllm_entry.py` wrapper that runs vLLM's own parser and
refuses reserved fields however they are spelled or supplied.

W4 landed locally: the host agent executes Park and Restore. vLLM parks with
`sleep?level=2` and restores with weight wake, `reload_weights`, KV wake and a
prefix-cache reset, followed by a fresh model probe before the gate reopens;
quiescence requires zero in-flight ingress work and zero running and waiting
engine gauges. An engine failure after dispatch leaves the launch uncertain and
quarantined until Terminate. SGLang park is wired through the adapter but refused
with no effect until a memory-saver observation source exists in production.
CPU and fake-engine tests only; no native engine has parked.

W11 landed locally. Stopping a role (server, host or standalone) is a signal, not a
command: new inference gets 503 `shutting_down`, admitted requests and streams get
up to the drain bound (default 30 s; now `shutdown.drain_timeout` in role
configuration, see below), engines are left running and owned,
and the next start re-attaches them. Standalone gained a SIGTERM handler and its own
re-attach path: a restarted standalone adopts its Ready embedded launches and
reopens dispatch only after matching process identity and an authenticated model
check. The explicit drain commands are `mllm drain host <name|id>` and, confirmed by
the owner, `mllm drain standalone`; both issue ordinary stops with verified cleanup
and leave deployments eligible for on-demand activation. Core suite 686 passing and
Clippy clean; fake-engine role tests only, not live. Known limits: standalone adopts
only Ready launches (not uncertain or parked ones); adoption on any role is refused
while request leases from the dead session remain, which a crash with requests in
flight leaves behind; the drain bound is environment-only; draining an offline host
waits for its deadline.

W8 landed locally: each host agent scrapes open, handle-bound engines'
`/metrics` on loopback with the per-launch key every second (vLLM running,
waiting and KV-usage gauges; SGLang equivalents once WE2 enables its metrics) and
sends bounded `ReportLoad` frames; the controller keeps the latest sample per
deployment generation, stale after 3 s and dropped when the host session ends, for
the router's instance selection (I3). Metrics stay unreachable through ingress.
Fake-engine and mTLS session tests only, not live.

W14 landed locally: deployment status, inspect and list views, and the host
inventory, carry a derived `development_controls` field marking every vLLM launch
with deep parking on and a parking residency as `exposed`, listing the reachable
surface and the mitigations and stating `production_safe: false`; text output
prints a notice on stderr. Nothing in configuration can set or clear the mark, and
unreadable state reports `unknown`, never safe. It found that before WE1 a
`restart_only` vLLM deployment could still launch in development mode; WE1's
derived sleep mode fixes that and a drift test guards it. Per-instance marking
waits for I1.

WE1 landed locally (ADR 0014, first slice). A deployment now carries
`engine_config`: typed common fields (dtype, quantization, KV-cache dtype, context
length, concurrency, CUDA graphs, language-model-only, trust-remote-code), a memory
request and KV size, per-family fields, and `extra_args` behind
`accept_extra_args`. Host profiles keep only host-fixed `args`; host `security`
gains `extra_args` (allowed by default), `approved_options` and `approved_paths`.
Reserved options are refused however spelled (abbreviation, negation, `=value`,
dotted keys, `--config`), and security-sensitive options need named host approval.
The memory request is declared or derived as weights plus KV plus a placeholder
8 GiB per-engine margin. vLLM sleep mode is now derived from deep parking and a
parking residency. Rendering of the new fields is WE2: until then SGLang stays on
its interim single pinned recipe and vLLM refuses typed fields it cannot yet render.
Core suite 708 reported (707 distinct), agent/config/protocol/domain/testkit 230,
CLI explicit targets and Clippy all pass; CPU and fake-engine only. WE1 changed the
stored format, so state written before it fails to load. The owner decided on
2026-09-22 that upgrades must migrate old state forward: pre-E1 effective
revisions and host documents are rewritten into the new shape, only unmappable
records are refused, and accounting for anything running is kept.

W12 landed locally. Host `eligible` is derived: online and reconciled, an accepted
approved configuration, and at least one reported profile matching an approved
profile's build fingerprint; a reported qualification grants nothing (ADR 0011).
Adoption after a crash now carries request leases on the same fence; dispatch
stays closed until leases from the dead session are closed on evidence (a fresh
probe plus a later quiescence observation from the same process group), never on
a timer. A restart also adopts a Stop the dead session left planned or uncertain
and completes it on gone evidence. `join host` accepts a relative `--join-file`.
Core suite 715 passing; Clippy clean. W12 found that production routers never write
`request_leases` (only tests call `grant_dispatch`), so durable in-flight accounting
does not exist in practice yet. The owner decided on 2026-09-22 that the router
writes a durable lease per dispatch and closes it on completion or cancellation
acknowledgement, with batched bounded writes, as SPEC §10 accounting requires.

Phase B passed live on 2026-09-22 with one server on control-host controlling both Sparks
(evidence `target/live/phase-b/`, all rows on a pre-WE1 source snapshot, qwen3-4b
only). SGLang ran natively on host-b for the first time (Ready in 127 s; answer, stream,
stop and verified cleanup). vLLM 0.29.0 ran remotely on host-a for the first time
(Ready in 29 s), and the development-control security check held: nothing
reachable from control-host or through ingress, every engine path keyed on loopback except
unkeyed `/health`, all engine sockets on loopback, and status marking the
deployment `exposed`. Both hosts then served concurrently with each route answered
by its own engine, and stopping one did not disturb 40 requests and a stream on
the other. Server SIGTERM during a stream finished the stream, answered new
requests 503 `shutting_down`, and re-attached the same engine after restart;
`drain host` stopped the engine with verified cleanup, and the next request
reactivated it on demand. Host agent SIGTERM kept the stream and engine and
re-attached, but new requests during the host's drain got 500 instead of 503
(open). vLLM on host-b was skipped: its `~/mllm-vllm-venv2` differs from host-a's
(`hf-transfer` extra, `jiter` 0.16.0 vs 0.17.0, a differently built
`instanttensor`), and no exception covers changing it. Three product bugs were fixed
with regression tests: admission compared the whole ledger against one host's
limits, so any charge on the other host blocked admission; a start accepted but
never armed before a server restart was orphaned and blocked all later commands;
and an operator start did not lift an earlier explicit stop, so drain looked like
an explicit stop. Open: host drain should suspend dispatch before closing ingress;
the same whole-ledger check remains in the policy-update overcommit and park/switch
admission paths, and `max_parked` counts across hosts; endpoint port leases are
global rather than per host; status shows `stopped` while a start is queued.

The pre-E1 state migration landed locally as store schema v19. It rewrites stored
effective revisions, retained sources, receipts and host publications into the
`engine_config` shape through WE1's own resolver, keeps binding identity on the
recorded legacy fingerprint so running launches stay recognised, and refuses
unmappable revisions with their bytes, bindings and reservations retained, a journal
entry, and an `operator_action` in status. Retained pre-E1 host journal commands
still decode, and Probe, Park and Restore of such a launch resolve against today's
approved document without re-signing. CPU tests only.

The owner granted a second scoped exception on 2026-09-22: create a separate
`~/mllm-vllm-0.29-venv` on host-b byte-identical to host-a's `~/mllm-vllm-venv2`, leaving
host-b's existing vLLM environments untouched and changing no driver or system
package. It was created the same day: the freeze (196 packages) and every
site-packages file match host-a by SHA-256, apart from venv-path shebangs and their
RECORD lines; the locally built `instanttensor` was copied as installed. `vllm
--version` reports 0.29.0 and CUDA imports work; no model was loaded.

WE2 landed locally. SGLang's single pinned recipe is gone: typed settings and extra
arguments flow into `ServerArgs`, only the reserved subset stays fixed,
`mem_fraction_static` is rendered from the memory grant, and `/metrics` is enabled
(loopback only; SGLang exempts it from its key). The SGLang entry re-parses extra
arguments with the installed `ServerArgs` parser and, after SGLang's own resolve,
refuses any change to a reserved field. vLLM now launches through
`runtime/vllm_entry.py`, which refuses `--config` and reserved fields however they
are spelled using the installed vLLM parser, then serves in-process; typed fields
render to their native flags. Every Spark runtime directory needs the new
`vllm_entry.py` before any vLLM launch. Until WE3 lands, SGLang launches verify no
checkpoint identity at all. Core suite 723 reported (722 distinct), runtime Python
281 and Clippy pass; CPU and fake engines only.

W2 landed: the live matrix harness in `scripts/live/matrix/` (snapshot, sync and
build; role bring-up with generated host documents and budgets; 20 deployment
fixtures across four models, both engines and both hosts; E0 evidence capture;
the I1 greedy-logprob identity probe; ownership-checked fault injection and a
bounded memory allocator; a load generator; and a row runner). It passed
shellcheck, syntax checks, config resolution through the real `mllm-config`
parsers, a fake-engine rehearsal and a dry run; nothing ran live. Gaps it found:
`mllm validate config` is still unimplemented; a `restart_only` SGLang launch was
reported refused, but that proved historical (see the policy-refusal paragraph
below); whether the
router forwards `logprobs` is unverified; the 8 GiB placeholder margin pushes the
4B and 14B fixtures past their declared requests until M16 measures real use.

Durable request leases and the Phase B fixes landed locally (store schema v21).
The router opens a durable lease before a request reaches the engine and closes it
on completion or proven non-acceptance; errors and timeouts leave it `uncertain`
and held, and streams hold it until the backend stream ends. A single group-commit
writer adds about 3.6 ms p50 and 8 ms p99 per dispatch under 32 concurrent
dispatchers (debug build). A host starting graceful shutdown now announces it; the
controller suspends that host's dispatch before its ingress closes, so new requests
get 503 `shutting_down` with `retryable: true`, and only the exact role-gate refusal
counts as not accepted. Policy-update overcommit and `max_parked` are scoped per
host. Endpoint port leases are keyed per host. Status derives `queued`, `starting`,
`stopping` and `reconciling` instead of reporting `stopped` or `ready`. Core suite
746 passing; CPU and fake engines only.

WE3 landed locally (store schema v20). A checkpoint's identity is the SHA-256 of a
sorted manifest of every file's path, size and hash, host-independent, computed
with a no-follow walk confined to the host's model store (in-store symlinks to
regular files only). Each accepted revision starts `pending`; the digest is
measured on the host by a new `DigestCheckpoint` action (or in-process for
standalone) and recorded before first launch, and a revision whose memory request
depends on weights stays provisional until then. Every launch and wake re-checks
file metadata and rehashes files up to 64 MiB, rehashing everything when any file
changes; a mismatch refuses launch, or leaves a parked launch parked. The old
pinned checkpoint manifest and preflight are removed, and runtime directories need
the new `runtime/pinned_file_observation.py`. Core suite 746 passing, runtime
Python 231; CPU and fake engines only. Open: a host refusing a launch for a digest
mismatch ends its session and the controller redelivers until the Initialize
deadline; a standalone model outside `MLLM_MODELS_ROOT` is now refused.

Policy refusals are now terminal answers (local, CPU and fake engines only). A
host that refuses a launch before any effect (checkpoint mismatch or unverified,
insufficient memory, residency tier, unauthorized) returns a typed refusal with a
closed reason instead of ending its session; the controller settles the launch at
once with that reason, and a refused Park or Restore answers `unchanged`. A
`restart_only` SGLang deployment resolves and renders without the memory saver,
and a Park of it is refused `unchanged`; the reported refusal of such launches came
from the pre-ADR 0014 launch shape. Standalone still forces SGLang to `deep`, so
`MLLM_DEEP_PARK=off` with SGLang was expected to be refused; it now falls back to
`restart_only` (`deployment_document` gained a `deep_park` argument). On
2026-09-22 the owner confirmed that `crates/mllm-cli/tests/live_interactive.rs` and
`.superpowers/sdd/2026-09-12-f2a2d-coordinator-integration/task-2-report.md`, which
AGENTS.md had excluded as the owner's, are leftovers from earlier agents. The
exclusion is removed, and they are deleted if no longer needed: validation goes
through the shipped product, not hardcoded scripts. `live_interactive.rs` was an
in-process vLLM park/wake lab that never ran the shipped binary; it is deleted,
and earlier mentions of it in this runbook are historical. The task-2 report,
whose unit committed long ago, moved to that slice's `archive/` directory. The
owner also decided that the remaining engine tests that bypass the shipped product
(`crates/mllm-cli/tests/live_vllm.rs`, `live_sglang.rs` and
`scripts/live/run-on-spark.sh`) become matrix rows driven through the product CLI
and roles, then are deleted; the temporary `repro_sglang_unarmed.rs` is already
deleted. That conversion is done: every scenario now maps to a product-driven
matrix row (M73 launch, inference, access control, stop and restart and memory
return for both engines; M38 empty model directory, recovery and an engine that
exits at once; M74 readiness deadline under `timeouts.initialize`; M75 standalone
refusing to boot without an engine, plus `check-release-clean.sh` in `sync.sh
build`), and the three files are deleted. None of the new rows has run live.

Owner decisions on open issues, 2026-09-22: (1) deployments gain
`timeouts.initialize` and `timeouts.wake`, defaulting to a value derived from
checkpoint size, with a per-command CLI override, replacing the fixed 900 s;
(2) the shutdown drain bound becomes a `shutdown.drain_timeout` field in server,
host and standalone configuration (default 30 s, at most 600 s), replacing
`MLLM_SHUTDOWN_DRAIN_SECS`; (3) SGLang's unauthenticated `/metrics` is accepted
because it is loopback-only read-only counters, and status marks it like the
development controls; (4) `drain host` on an offline host returns at once with
pending stops that complete with gone evidence on reconnect, `--wait` still
waits, and the host takes no new placements while a drain is pending; (5) waking a
launch parked before checkpoint digests existed first measures and records the
digest, then wakes. A Tailscale SSH re-authentication prompt briefly blocked live
runs; the owner cleared it the same day.

Fixes landed locally (CPU and fake engines only): `mllm validate config` validates
server, host, standalone and deployment files offline through the product's own
parsers, optionally resolving a deployment against a host document; non-stream
responses no longer drop `logprobs` (the request body was already forwarded
untouched); remote bindings are no longer test-bound on the controller; hosts
accept `load_report_interval` (250 ms to 5 s); the host agent and standalone refuse
to launch from a runtime directory or module that is a symlink, not owned by the
running user, or group/other-writable (`runtime_integrity`); `shutdown.drain_timeout`
replaces the environment variable; SGLang status marks the unauthenticated
loopback `/metrics`; and ADR 0014 is Accepted with SPEC §8.2 and §16.3 amended.
Known consequences: a checkout whose `runtime/*.py` files are group-writable (0664,
as on control-host) can no longer boot standalone from that checkout, and every
`docs/examples/*.yaml` file fails the product parsers because they are stale
sketches. The owner decided the runtime check should relax to owner-only: group
write is allowed when the group is the owning user's private group (umask 002
style), and remains refused otherwise.

Offline drain and legacy wake landed locally (store schema v24, `host_drains`). A
planned cleanup for an offline host is deferred rather than armed, so it no longer
halts the coordinator after the 30 s protocol timeout, and other hosts' cleanups
are not blocked behind it. `drain host` on an offline host returns at once with
`host_state: "offline"`, `stops: "pending"` and operation ids; `--wait` polls. A host
with a pending drain is excluded from eligible hosts. Waking a pre-digest parked
launch measures the digest first; a Restore now carries the recorded digest, and a
mismatch refuses without any engine call. Limit: a drain completes only if the
host reconnects within the Stop deadline (request time plus 900 s); after that the
Stop stays planned, the engine stays charged and the host stays ineligible. The
owner decided that on reconnect the server closes such expired, never-armed stops
as `expired` and issues fresh stops with new deadlines, keeping accounting until
gone evidence, so drain intent survives an outage of any length.

Live M16 (per-model smoke) started 2026-09-23 on both Sparks. Its first run found a
product bug, fixed with a regression test: a host agent ended its control session
when a launch failed before readiness, so the controller waited the full Initialize
deadline and tore down the host's other effects; the failure is now a journaled
result. The engine error behind it was SGLang with no memory left for KV cache
under a 16 GiB request minus the 8 GiB placeholder margin, so harness requests rose
to 20 GiB (4B) and 42 GiB (14B). It also showed that a failed deployment's name
cannot be reused until undeploy exists (W6, now in progress). Further gaps from
the same run: a failed deployment cannot be stopped (`Lifecycle state does not
permit this action`); a memory request smaller than weights plus KV plus margin
still resolves and then fails inside the engine; and W8 load samples appear in no
CLI status view.

W6 undeploy landed locally. `mllm undeploy model <name|id>` is refused with 409
`undeploy_requires_cleanup` while any instance holds a runtime, reservation, lease,
open step or operation; once everything is released it removes routes, instances
and checkpoint digest rows in one transaction, turns the deployment into a
tombstone so the name can be redeployed under a new ID, keeps all history, and
never touches model files. The router answers 404 for the removed model at once.
Replays by request id return the original receipt. Core suite 795 passing; not
live. The owner decided on 2026-09-23 to rename the command `mllm delete
deployment <name|id>` (dropping `undeploy model`, with SPEC §6.3 and §14 amended)
and to add `--stop`, which stops every instance, waits for verified cleanup and
then deletes, durably and replayably, reporting `pending` if cleanup cannot yet be
proven.

Remote co-residence landed locally (host journal v4, store v25). An enrolled host
now holds one launch claim per instance incarnation and advertises
`launch_claims: per_launch`; before each launch it re-checks, against its own
approved policy, that the new launch fits beside its claimed launches (typed
refusals `insufficient_memory`, `device_conflict`, `port_conflict`), charging each
claim by its durable phase and charging an unresolvable retained claim the whole
budget. Readiness authority and gates are per launch. Different deployments now
co-reside on one remote host; a fake-engine end-to-end run served two deployments
from one enrolled host with independent stops. Core suite 798 passing; not live.
Open: two instances of the same deployment still cannot share a remote host,
because the host fences commands per deployment by generation, contrary to the P1
decision that instances may share a host; a host does not re-check wake growth
beside other claims.

I3 landed locally. The router balances each request across a deployment's
instances whose gates are open (remote ones also need a live host session). Score
= max(router in-flight, engine running + waiting) when a fresh matching W8 sample
exists, otherwise router in-flight, plus a penalty of up to 8 above 80% KV use;
ties rotate deterministically, and every choice is logged as a `router_selection`
line. The durable lease is fenced on the chosen instance's generation in the grant
transaction, closing the earlier lease-versus-forwarder race. Failover happens only
before the engine accepts the request (at most four attempts; streams before the
first byte); anything else stays uncertain and is never replayed. A binary test
with a real server and two host agents spread a burst across hosts, steered away
from a host reporting high load, did not replay a request whose host agent was
killed, and rejoined the adopted engine. Core suite 798; not live. Open: a frozen
(SIGSTOPped) host agent still accepts connections until its control session is
declared lost, so requests routed there in that window hang until the 300 s
forward timeout. The owner decided on 2026-09-23: server and agent exchange
heartbeats every second on the control session; after 5 s of silence the server
suspends dispatch to that host (accounting kept, nothing released), and after 30 s
it treats the session as lost; both values are server configuration.

Runtime-integrity relaxation and drain re-issue landed locally. The host agent's
runtime check now allows group write only when the group is verifiably the owning
user's private group (name, primary gid, no members, no other account using it),
refusing on any failed lookup; other write, symlinks and foreign owners stay
refused. Other group-write checks were not relaxed: the SGLang entry path check in
`crates/mllm-adapters/src/sglang/args.rs`, launcher ownership and observation
checks, and the runtime Python checks. An expired, provably never-sent drain stop
for a reconnected host is now closed as `expired` and re-issued with a fresh
deadline in one transaction, keeping binding, lease and reservation until gone
evidence. Core suite passing; CPU and fake engines only.

Owner decision on 2026-09-23, after reviewing which files the permission checks
guard: mllm's private state (identity, credentials, locks, observation sockets)
stays strict; mllm's own runtime helper scripts use the owner-only rule everywhere
(including the SGLang entry path check); and engine installation files get no
hard-coded hashes and no permission rule. Instead an installation's fingerprint
(version plus a digest of its files) is recorded at registration with drift
flagged later, and mllm's SGLang hooks probe the internals they need at launch
(API shape, not file hashes), refusing only the dependent feature, such as deep
parking, when a build lacks them. The pinned SGLang 0.5.20 source audit, which
refused any custom or patched SGLang build, is replaced accordingly (ADR 0008).

Deployment timeouts landed locally. `timeouts.initialize` and `timeouts.wake` sit
beside `request_deadline`, outside the recipe fingerprint. Derived placeholders:
initialize = min(120 s + 10 s per GB of weights, 1800 s) and wake = min(60 s + 5 s
per GB, 900 s), or 900 s while the digest is pending, never beyond the request
deadline. Effective configuration and status record values and provenance;
`--initialize-timeout` overrides per command; start and stop windows replace the
fixed CLI 900 s. `timeouts.wake` bounds nothing until a coordinator wake exists.

`mllm delete deployment <name|id> [--stop]` replaced `undeploy model` locally,
with SPEC §4.3, §6.3 and §14, ADR 0013, the plan and the matrix updated. `--stop`
issues an administrative stop (so on-demand activation cannot restart it before
deletion), waits for cleanup, then deletes; it is journaled in two steps and
resumable by request id, and returns `deleted:false, cleanup:"pending"` (exit 0)
when a host is offline or cleanup is not yet proven. Core suite 798; CLI 113.

Per-instance host fencing landed locally (host journal v5, store v26, additive
`CommandIdentity.instance_index`, capability `launch_claims: per_instance`). The
host fences each instance of a deployment against its own last assignment, so two
instances of one deployment now co-reside on one enrolled host; a fake end-to-end
run brought both to Ready, stopped them independently, and admitted an instance
restarted below its sibling's generation. Older hosts keep the same-deployment
refusal and now draw a fresh generation for a returning instance. The host also
re-checks a wake beside its other claims and refuses `insufficient_memory` with
the launch left parked. Core suite 800; not live.

Engine installation fingerprints and capability probes landed locally. The pinned
SGLang source audit and the saver source audit are deleted. `runtime/engine_capabilities.py`
probes by API shape (`core`, `deep_park`, `metrics`, and `observation` for SGLang)
against the real SGLang 0.5.20 and vLLM 0.29.0 layouts; a missing `deep_park` refuses
only `deep` launches and Park (`capability_missing:deep_park`, suggesting
`restart_only`), and a missing `core` refuses every launch. The host agent records
each installation's version and a file digest at start, re-measures at launch,
flags drift in status and the journal, and refuses `installation_drift` only when
the profile sets `security.installation_drift: refuse` (default `warn`). mllm's
helper scripts share one owner-only rule (Rust and a Python mirror); private state
stays strict. SPEC §8.1, §9.2, §13.3, T22 and T37 and ADRs 0008 and 0014 are updated.
Core suite 832; runtime Python 225; CPU and fake engines only. Remaining small
items: standalone records no installation fingerprint yet; `engine_capabilities.py`
is not yet a required runtime file; the SGLang descriptor still carries the old
`source_revision` token; the probe's 120 s limit is unverified on a Spark; and the
`roles_f1` tests collide on port 8100 when run in parallel.

Control-session heartbeats landed locally (additive protocol, negotiated so older
peers are never suspended for silence). Server and agent heartbeat every second;
after `control.heartbeat_suspend_after` (default 5 s) of silence the server marks
the host unresponsive, forgets its readiness proofs and suspends its dispatch and
placement eligibility without releasing anything; hearing it again requires a
fresh probe before dispatch reopens; after `control.heartbeat_lost_after` (default
30 s) the session is lost. An agent that stops hearing the controller reconnects
without touching engines. In a binary test a SIGSTOPped host agent was suspended
after about 4.8 s, new requests went to the other host, the in-flight request kept
its lease and completed once without replay, and after SIGCONT the same engine
served again; a frozen server made both agents reconnect with engines kept. Core
suite 832; not live.

W5 landed locally: park, wake and preinitialize run through the coordinator as
durable operations on an instance's retained binding, so a parked instance always
wakes on its own host with the same binding and generation. Each transition is
budgeted at its peak before arming and settled on evidence; park drains to zero
request leases first; refused transitions keep state and footprint; uncertain ones
keep peak reservation, claim and closed gate until a stop settles them on gone
evidence. `max_parked` and parked budgets are enforced by stopping the least
recently parked instances on that host, never ready work. `park deployment`,
`start deployment` (wakes parked instances first), on-demand wake (concurrent
requests join one restore), `preinitialize deployment` (one instance at a time)
and controller-owned idle timers (`lifecycle_defaults.ready_idle_timeout` and
`parked_idle_timeout`, off when omitted) are wired; restarts adopt parked launches.
Core suite 833; scripted hosts only, no engine parked. Open: requests arriving
while an instance is parking or waking get a retryable refusal, whereas SPEC §6.1
says PARKING and WAKING queue; SGLang park stays refused until a memory-saver
observation source exists; standalone has no idle configuration. The owner
decided on 2026-09-23 that idle timers stay off unless configured.

M16 (per-model smoke) passed live on 2026-09-23 for all ten model and engine
combinations across both Sparks (run `matrix-20260923T034935Z`, snapshot `f5d793ea`,
evidence `target/live/matrix/M16-*`), five of them only after a variant or rerun.
Every row answered 3/3 prompts, forwarded logprobs and left zero request leases.
Ready times ranged from 29 s (vLLM 4B) to 585 s (SGLang 27B BF16); checkpoint
digests took 7 to 32 s on first measurement and under 1 s after. Measured
suggested requests: 4B 19–24 GiB, 14B 42–46 GiB, 30B-A3B 74–82 GiB, 27B BF16
71–78 GiB, 27B NVFP4 about 39 GiB steady. Engine and recipe findings: vLLM 0.29 on
qwen3-30b-a3b ran the whole host out of memory during FlashInfer MoE JIT compilation
(the kernel OOM killer also killed the host role) and passed with
`--moe-backend triton` through extra arguments; SGLang 0.5.20 refuses
`--language-model-only` for `Qwen3_5ForConditionalGeneration`, so 27B on SGLang
passes without it; both NVFP4 rows briefly drove MemAvailable to about 0.5–6 GiB
during startup JIT before settling near 35 GiB, so a request sized from the
transient peak (about 130 GiB) is misleading. The I1 identity check was void for
one pair: 27B BF16 on SGLang and 27B NVFP4 on vLLM produced identical greedy text
with different logprobs. Product findings: a host result reporting a launched but
dead engine was discarded, so the controller waited the full Initialize deadline
(fixed locally with a regression test; not yet live); and the coordinator runs one
worker loop for all hosts, so a slow or dead activation on one host delayed stops
and activations on the other by up to 15 minutes, causing both cleanup-timing
failures and five `Endpoint capacity is unavailable` rejections.

W13 landed locally: the launcher records each spawned child's exit by exact
identity; the host agent watches Ready launches every 250 ms, closes the gate and
sends `MemberExit` (repeated every 5 s until settled); the controller closes
dispatch at once, journals `engine_exited` and issues an ordinary stop that
terminates any surviving group members and releases only on gone evidence; status
shows `failed`; the next request relaunches on demand. In end-to-end fake runs
dispatch closed within 2 s of SIGKILL. The cleanup pass also landed: standalone
records installation fingerprints and drift (`MLLM_INSTALLATION_DRIFT`),
`engine_capabilities.py` is a required runtime file where used, the SGLang
`source_revision` token is removed (binary and runtime directory must now be
updated together on each Spark), standalone engine ports are configurable
(`MLLM_STANDALONE_ENGINE_PORTS`, which also removed the CLI test port collisions),
and `docs/examples/*.yaml` are rewritten and validated by a test. Core suite 836.

Owner decisions on 2026-09-23 from M16: lifecycle work becomes per-deployment
concurrent, so waiting on one deployment's load never blocks another deployment's
start or stop, while admission and reservations stay serialized through store
transactions; and startup gets its own memory budget, declared or measured on first
run, reserved until the instance is Ready and then dropped to the steady request,
with launches on one host serialized through their startup phase whenever their
peaks do not fit together (ADR 0007 phase-aware admission).

The SGLang memory-saver observation source landed locally, so the earlier notes
that SGLang park stays refused are superseded (pending live proof). Read-only
inspection of host-a showed that torch-memory-saver 0.0.10 exports no snapshot
API and that SGLang 0.5.20 `ServerArgs` is a msgspec struct, so the source reads
the saver's per-tag memory pools and asks the CUDA driver whether each segment is
still mapped. The engine enrolls observation from inside its scheduler process
(owner-only record and socket in a 0700 directory, requests authenticated with a
key derived from the launch's admin key, so a restarted host can still observe its
launch). Park counts as released only when every `kv_cache` and `weights`
allocation is observed unmapped; partial observations are refused before an engine
call and uncertain after one. Quiescence needs zero in-flight ingress plus zero
SGLang running and queued gauges. Embedded standalone uses the same observer. Core
suite 847; runtime Python 238; CPU and fakes only. The harness now uses M16's
measured requests and working recipe variants. Live questions remain: whether the
driver reports paused segments as unmapped on GB10, segment counts for the large
models, and that CUDA-graph memory is neither observed nor released by SGLang park.

With M16's measured requests (4B 24 GiB, 14B 46, 30B-A3B 82, 27B BF16 78, 27B NVFP4
40) the planned co-residence pairs no longer fit the 80% managed limit (about
97 GiB). The owner decided on 2026-09-23 to keep 80% and give co-residence
fixtures a smaller declared KV cache and context, while single-model rows keep
full KV. Engines preallocate their KV pool, so a smaller pool trades concurrency
and maximum context for density without making accounting uncertain; the
remaining uncertainty is startup peaks (covered by the startup budget), the
placeholder per-engine margin, and memory outside the pool (absorbed by the free
reserve and the host's published available memory).

Per-instance concurrent lifecycle landed locally (ADR 0015). A scheduler discovers
work and runs each effect (initialize, cleanup, park or restore, settlement) as its
own task per instance lane, with separate bounded pools for activations and
cleanups (`max_concurrent_effects`, default 8), so stops never wait behind loads
and a hung load on one host no longer blocks others; admission and ledger stay
serialized through store transactions, retry cooldowns hold only their own start,
and shutdown joins every task. Because host ingress is keyed by deployment and
member rather than instance, at most one Initialize per deployment and host runs
at a time. Core suite 847, CLI 128 (including the two-host tests) and Clippy pass;
not live.

W10 request-driven switching landed locally. A request for a deployment with no
open instance now waits in a bounded queue (per-deployment and total counts,
buffered bytes, deadline) and joins one activation instead of being refused, also
while an instance is starting, waking, draining, parking or stopping. When the
on-demand start is refused for capacity, the switcher plans on the host needing
the fewest evictions, takes that host's first-come turn, keeps a busy last-ready
victim admitting for the non-resetting admission window, closes its gate, waits for
its request leases to drain (the switch fails and the gate reopens on drain
timeout; nothing is killed), parks deep victims or stops `restart_only` ones and
waits for verified release, then activates the target. Victims serving elsewhere
go first, then least recently used. Switch events and journal entries record each
step. Core suite 860; fake engines only. Gaps: status does not show a switch in
progress; queue limits and drain timeout use built-in defaults rather than host
queue policy and configuration; warm-residency commitments (SPEC §6.5) are not
excluded from victims; on a single-claim host the planner ignores host occupancy;
and a failed switch can reopen a gate that a host-loss closure closed during the
drain window, which must be fixed. The owner decided on 2026-09-23 that an
explicit `start deployment` never evicts unless given `--evict`, which runs the
same switch plan and reports the victims.

The startup budget, per-instance host ingress and co-residence fixtures landed
locally (store schema v27). Deployments may declare `engine_config.memory.startup`;
otherwise a first-run measurement per revision, host and installation (recorded
only when no other launch was on the host) is reused, or a placeholder of
max(request, weights × 1.6 + 8 GiB) applies. Admission reserves the startup peak as
the cold phase until Ready, then the steady request. A per-host activation gate
holds a start whose peak does not fit beside in-flight peaks but would once they
are Ready. Host ingress is keyed per instance, so two instances of one deployment
on one host start concurrently. Co-residence fixtures (`--co`: 8192 context, small
KV) fit the planned pairs within 97.35 GiB. Core suite 860, CLI 130 and Clippy
pass; fake engines only. Problem: the placeholder startup peak for the 30B-A3B
model (99 GiB) exceeds the managed limit, so it can never be admitted to be
measured. The owner decided on 2026-09-23 that an unmeasured model whose estimate
exceeds the limit may start only alone on its host (emptied by the normal switch
rules if needed), reserving the whole managed limit; that run is measured and
later starts use the real peak.

That fix pass landed locally (store schema v28). A solo first start reserves the
whole managed limit, is refused with `startup_requires_empty_host` while any other
engine holds a charge, is made room for by request-driven switching or by an
explicit `start … --evict`, and records its measured peak. `start deployment` and
`start instance` accept `--evict`, journaled and replayable, reporting victims and
the switch id; default start never evicts. Gate closures now record their reason
(`switch`, `host_session`, `engine_exit`), so a failed switch reopens only its own
closure and a passing host probe does not reopen a gate a switch holds. Switch
drain timeout (`switching.drain_timeout`) and queue limits come from configuration,
status shows a switch in progress, and a new `lifecycle.warm: true` flag exempts a
deployment from switch eviction, idle policy and parked reclamation (ADR 0013
amendment). Single-claim hosts are freed by releasing their occupant. The
standalone lab entry points and deprecated park-policy aliases are removed. Core
suite 869, CLI 126 and Clippy pass; fake engines only.

The owner asked on 2026-09-23 for a dedicated performance benchmark row (M80),
driven through the shipped router: per-request time to first token, time to last
token, prefill and decode rates and inter-token latency percentiles across prompt
lengths and concurrency for every model on both engines, router and ingress
overhead against direct engine calls, and the lifecycle latencies users feel (cold
start, wake from park and switch, each measured to first token). It runs after the
current live phase. M80 (`scripts/live/matrix/bench.py`, `rows/M80.sh`,
`bench_report.py`) is built and validated against a fake streaming server; it
never reads engine secrets. The owner decided the same day to track whatever the
engines provide and otherwise measure at the mllm level: mllm's own router and
host ingress record per-request timings (queue wait, activation wait, forwarding,
upstream first byte, total), and the host agent forwards engine latency histograms
from the metrics it already scrapes where an engine exposes them, so the path
overhead can be separated from engine time for both engines through the product.
That instrumentation landed locally: the router records ten per-request phases
(queue wait, activation wait, selection, lease grant, forwarding, upstream first
byte, first content, last chunk, total) per deployment, instance, generation and
engine; host ingress records time to headers, first and last byte; the host agent
forwards bounded deltas of the engines' own latency histograms (vLLM 0.29: TTFT,
end-to-end, queue, prefill, decode, inter-token; SGLang 0.5.20: TTFT, end-to-end,
queue, inter-token) with its load reports; `GET /management/v1/metrics/latency` and
`status`/`inspect deployment` expose each series with its tier and source; and
`observability.timing_header` (off by default) adds per-request timings. M80 reads
them through the CLI. Core suite 877 and Clippy pass; fake engines only; not live.

The two-host live matrix ran on 2026-09-23 (evidence `target/live/matrix/`). Passed
live: M75 and M73 on both engines (launch argv, loopback-only listeners, keyed
engine routes, restart as a new generation, memory return); M05 and M08
(development controls marked `exposed`, SGLang `/metrics` marked, router never
serves engine paths, ingress refuses unkeyed calls); M29 vLLM park and wake on both
hosts (sleep level 2, weight wake, reload, KV wake and prefix reset with the same
processes; about 90% of the ready footprint released; wake on request in 60–70 s);
M28 SGLang park and wake (saver mapping observed going from 39 GB to 0, same
processes, 87.5% released, wake 182 s); M30 park waiting for a stream (SGLang); M32
`max_parked` stopping the least recently parked; M33 preinitialize (SGLang); M34
refusals for `restart_only`, `host_backed` and opted-out hosts; co-residence of vLLM
and SGLang on one host (M19/M22); two instances of one deployment on one host and
across both hosts, including `stop instance`/`start instance` and count 2→1→2 via the
new `deploy model --revision`; balancing (M54 10/10 split, M56 31/33 at 32
concurrent); a frozen host agent suspended after about 5.2 s with no replay (M58)
and rejoining after a fresh probe (M60); engine SIGKILL settled in 1.4 s and
relaunched on demand (M36, M37); the readiness deadline (M74); M38; host agent and
server restarts re-attaching the same engines (M40, M42); `delete deployment
--stop` and `drain host` (M64); request-driven switching same-engine and
cross-engine with correct models at every step (M27, M31) and `start --evict`.
The full workspace suite passed 1486 on host-a; on control-host `a1_gate` and three
standalone tests fail only because that machine's small free memory cannot admit
the fake engine under standalone's 50% policy. Eight product bugs were fixed with
regression tests: an SGLang start beside another loading launch (starts are now
serialized through startup when SGLang is involved), `--mllm-` extra arguments
passing resolution, the SGLang saver library refused as a hard link, SGLang disk
reload renaming the served model, a republished host policy being ignored, a
memory-neutral park blocked by the free-memory check, a fixed 300 s SGLang reload
cap, and the missing revision-aware CLI update. Open findings: the SGLang scheduler's
torch distributed store listens on all interfaces and accepted a connection from
control-host (security); the router's fixed 300 s stream cap cut a long stream and left
its lease uncertain, so a park never armed (M30 vLLM); switching reclaims its own
parked target under `max_parked 1`, so every switch was a cold restart (M31); a vLLM
wake beside a ready SGLang 14B was refused for resources despite fitting (M33); the
solo first start never triggers while a digest is pending; `queue_full` answers
413; `deploy` hides refusal reasons; `inspect deployment --effective-config` is
unsupported on the server role; M57 and M59 steering was not demonstrated. Not
run: M35, M39, M41, M43–M47, M51–M53, M55, M61–M63, M65–M72, M80.

All nine open findings were then fixed locally with regression tests (store schema
v29). Security: torch 2.13's `TCPStore` listens on every interface regardless of
host, so SGLang now uses a file rendezvous in a 0700 directory with Gloo and NCCL
pinned to `lo`, `nccl_port` is reserved, and vLLM pins its host IP and interfaces to
loopback as defence in depth; M08 rerun live on host-a showed only 127.0.0.1
listeners. Streams are no longer cut at fixed wall-clock caps: a stream ends only
when its first event misses the request deadline or a later gap exceeds
`resource_policy.queue.stream_idle_timeout` (default 120 s). A switch no longer
reclaims its own parked target. The M33 refusal came from admission charging
resident engines twice (published free memory already excluded them); hosts now
report per-process resident memory keyed by process identity, and admission credits
Ready owners' verified resident memory, bounded and never beyond their reservation
(ADR 0007). Weights are sized by a stat walk before the full digest, so the startup
estimate and the solo first start work while the digest is pending. `queue_full`
answers 429 with `Retry-After`; `deploy` names its refusal reason;
`GET /management/v1/deployments/{id}/effective-config` and `inspect deployment
--effective-config` work on the server with secrets redacted; and CLI tests no
longer depend on the machine's free memory. Workspace 1527 tests, core 886,
runtime Python 253 and Clippy pass. Still to prove live: warm switching (M31), the
M33 wake, the solo first start and long vLLM streams (M30). A signalled SGLang stop
leaves its rendezvous directory behind (open).

Live reruns and the benchmark on 2026-09-24 (run `matrix-20260923T234659Z`,
evidence `target/live/matrix/`) proved those fixes: M08 showed only loopback
listeners and no rendezvous directory left after any SGLang stop (the host agent
now owns `<state_dir>/rendezvous/<incarnation>` and removes it on gone evidence);
M33 admitted the vLLM wake beside a ready SGLang 14B; the solo first start refused a
plain start, evicted with `--evict`, reserved the whole host and recorded a
measured 82.69 GB peak; a 3000-token vLLM stream ran 379 s uncut and the park armed
0.9 s after it; warm switching kept the same processes across three cycles (M31);
long-prompt skew (M57) and an engine stall (M59) steered new work away from the
loaded or stalled instance with no replay; M54, M56, M58, M60, M43, M68 and M69
passed; M41 settled a launch whose agent died at spawn only at the Initialize
deadline. Product failures still open: stop does not drain request leases before
terminating, so retiring a replica by count (M65) or `delete --stop` (M66) cut
in-flight requests, although SPEC §6.3 says stop drains; stop during Initialize is
refused (M47); a failed deployment cannot be stopped (M53, fix in progress); the
per-request timing header labels the engine `model`; and SGLang 0.5.20 cannot
reload the modelopt NVFP4 checkpoint from disk, so every deep wake of that recipe
fails (left uncertain with its reservation until stop proved it gone). Two
latency-view bugs found during the run were fixed. M61 (mixed-engine replicas of one
route) is not expressible by design (Q6). Not run: M35, M44, M45, M46, M51, M52,
M62, M67, M70–M72.

M80 results (2048-token prompt, one request, through the router): decode rate
tracks model bytes on the GB10, about 21 tokens/s for 4B, 8 for 14B, 4.4 for 27B
BF16, 10 for 27B NVFP4 and 30 for 30B-A3B; time to first token 0.3–2.2 s. vLLM cold
starts are much faster than SGLang (4B 22 s against 68 s; 30B 85 s against 378 s);
vLLM wakes from deep park in 8–81 s, while SGLang's disk-reload wake is close to a
cold start for large models. mllm's path adds about 20–60 ms (vLLM) and 40–100 ms
(SGLang) to time to first token, most of it router-to-ingress and ingress-to-engine
time growing with prompt length; selection is under 1 ms. Reports:
`target/live/matrix/M80-report.md` and `M80-overhead.md`.

The stop-related failures were then fixed locally with regression tests. An operator
stop is always accepted: with nothing held it is recorded at once (stop from a
failed or never-started deployment now succeeds, replacing the old rule that stop
from stopped is illegal); with a runtime held it runs ordinary cleanup; during an
unassociated Initialize it is deferred and issued once the launch settles. Every
ordinary cleanup now drains first, waiting for the instance's in-flight request
leases up to `switching.drain_timeout` (default 30 s) before terminating, which
covers count-decrease retirement, `delete --stop` and drain host. The timing header
names the host-reported engine. SGLang with a modelopt or NVFP4 quantization is
refused `deep` residency (`capability_missing:deep_park`, suggesting
`restart_only`) because SGLang 0.5.20 cannot reload it from disk. Workspace 1545,
core 896 and Clippy pass; fake engines only, pending live recheck of M47, M53, M65
and M66. The live recheck on 2026-09-24 (run `matrix-20260924T121812Z`) passed:
stop during Initialize was deferred and completed on both engines (M47); a plain
stop of a failed deployment was recorded at once and `delete --stop` worked (M53);
reducing the instance count under 12-way load returned 616 of 616 requests with the
retiring instance drained first (M65); `delete --stop` during a long stream waited
the full 30 s drain bound on both engines (M66); SGLang NVFP4 `deep` was refused
before any process started while `restart_only` served; and M73 passed on both
engines. Open: the refusal reason reaches only the server journal (status shows
`failed` without it, although SPEC §6.4 requires status to expose the latest error);
a switch to a target that needs an empty host parks the incumbent, waits 30 s and
then stops it, with a misleading uncertainty message.

I2 landed locally (store schema v23; new `mllm-scheduler` placement). Each
instance carries its own revision, generation and state; a count-only revision
leaves running instances untouched, adds instances without eviction and retires
surplus ones with verified cleanup; any other revision stops each instance and
restarts it on the new revision, durably. Placement runs in the start transaction
with spread or pack, `max_per_host`, deterministic ties, host-label selectors
(`resource_policy.labels`) and per-host ledgers; unplaceable instances defer with a
diagnostic. Deploy resolves against every allowed host and records refusals.
On-demand activation starts the lowest instance not operator-stopped; explicit start
brings up all. Status aggregates instances (`ready` if any is ready, `failed` only if
all failed) and lists hosts and per-instance errors. Core suite 785 passing, Clippy
clean on all crates, and a two-host fake end-to-end test passes; not live. Open: a
remote host still holds one launch claim in its journal, so a second instance on
the same enrolled host is refused `host_occupied` (co-residence works only on the
embedded host); the router still picks the lowest ready binding rather than
balancing (I3); a parked instance's placement is not sticky (W5). Core suite 760 reported (759 distinct); runtime Python 231
(the drop from 281 is WE3's removal of the old checkpoint preflight tests).

I1 landed locally (store schema v22; ADR 0013 accepted with the Q7 amendment; SPEC
§1.1 R06, §2, §10 and §16.4 amended). Deployments accept `instances` (default 1,
at most 64) and `placement` (`hosts`, `selector`, `strategy: spread|pack`,
`max_per_host`), with `host` as shorthand; unplaceable or contradictory shapes are
refused by name. The store records instances, per-host effective revisions and
per-instance bindings, runs, claims, leases and owners; existing deployments
migrate to instance 0 with their accounting intact. Status reports desired and
ready instances, a `degraded` condition and per-instance state with its own
development-controls mark. `stop instance <deployment>/<n>` and `start instance
<deployment>/<n>` exist in CLI and API. The lifecycle still realizes only instance 0:
placement of further instances, resolution against every allowed host, count
changes while running, Q8 stop-all-then-start orchestration, per-instance
on-demand choice, host label matching and per-instance status derivation are I2.
Core suite 760 reported; CPU and fake engines only.

W1 landed locally: deep parking is enabled by default and a host opts out with
`security.deep_park: disabled` (standalone: `MLLM_DEEP_PARK=off`). SPEC §9.1, §16.2,
§18, T21 and AGENTS.md now say so, with ADR 0012 recording the decision. This is not
a production-safety claim: vLLM development-mode controls stay loopback-only behind
the per-launch key guard and are never reachable through ingress or the router.
A `restart_only` deployment never receives a park policy. Status does not yet mark
profiles that expose these controls (open). W3 landed locally: session protocol
version 2 adds Park/Restore member actions, residency evidence, bounded load
reports and member-exit reports; command encoding stays at version 1 so retained
host journals remain readable. After both, the core suite reports 675 (674
distinct) passing and Clippy is clean. None of this is live-verified.

Recovery gaps U5-G1…G3 are fixed locally (not yet live): an uncertain remote launch
is settled by an authenticated host Terminate with gone evidence or stays uncertain
with accounting retained; operator stop accepts it; a restarted controller adopts
such launches. Host session loss suspends remote dispatch (router answers 503),
and a new `Probe` action re-proves readiness with a fresh native model probe
against the identical owned processes before dispatch reopens; server restart uses
the same path. Status now reports `uncertain` rather than `stopped`. The core suite
(673 reported) plus agent/config/protocol tests and Clippy pass. Known remaining
limits: a launch whose host agent dies mid-Initialize waits for the Initialize
deadline before settlement; adoption refuses deployments with in-flight request
leases or a prior-session cleanup step; host journal history is never compacted.

U5 remote single-host SGLang passed live on 2026-09-22. The shipped roles ran with
the server on control-host and an enrolled host on host-a: init, invite, join, deploy
with an explicit `host:` selector, activation, routed authenticated inference
through the host's private ingress, stop, and verified cleanup across three
generations of deployment `01M356HDG005QA16Q7EA617KZA` (answers 42, 63, 42;
streaming returned 200). Readiness came from the host's native model probe. Each
stop left no engine process group, no GPU compute process, and zero reservations,
leases and claims. Unauthenticated router calls got 401, direct ingress without
the gate key 403, inference after explicit stop 429 without autoactivation, and a
replayed stop request returned the original operation. Non-secret evidence is in
`target/live/u5-remote-0292/`. The first live run found two product bugs, both
fixed with a regression test (T09/T33/T38): controller command redelivery every
500 ms spawned duplicate host effects until the session was torn down mid-launch,
and every reconnect republished the stale startup inventory, which publication
refused after its 2 s freshness window. Controller (237) and agent (40) tests and
their Clippy pass; the full core suite was not rerun by that unit. Open U5 gaps:
G1 a remote launch that goes uncertain cannot be stopped or settled (the first run
ended with an abandoned uncertain reservation in its isolated state directory,
after the engine group it had started was terminated by signal); G2 after a host
restart, a Ready remote deployment stays ready at the controller while the host
gate returns 500; G3 status can report `stopped` while an uncertain engine runs;
G4 host `eligible` is hard-coded false. Controller restart with a live remote
deployment, stream interruption and CLI crash recovery were not exercised.

The D5 model set is in place on both Sparks with identical payload SHA-256:
`~/models/{qwen3-4b-instruct, qwen3-14b, qwen3-30b-a3b, qwen3.8-27b,
qwen3.8-27b-nvfp4}`. The first four were copied from host-a over the direct link;
`qwen3.8-27b-nvfp4` was materialized on each host from the complete Hugging Face
snapshot `009632fef96dd349150baa780c984e62e70e91fe` of
`RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead`. The anchor is a hybrid multimodal
`Qwen3_5ForConditionalGeneration` checkpoint; the NVFP4 build is a modelopt mixed
NVFP4/FP8 quantization with an FP8 KV-cache scheme and is 23.75 GB on disk, not
the catalog's 32 GB. Whether the installed SGLang 0.5.20 and vLLM builds can serve
this architecture and quantization has not been checked by any engine run.

The U5 recovery fixes passed live on 2026-09-22 on native SGLang 0.5.20 (server
control-host, host host-a, deployment `01M35ARNS85PT8D1WYHXK7EFDC`; evidence in
`target/live/recovery-0292/`). A killed host agent made the router answer 503
within 1 ms, and the restarted agent re-proved the same engine processes and
resumed serving in under 1 s. When the engine had died meanwhile, dispatch stayed
closed and stop cleaned up. A server restart adopted the Ready launch and resumed
serving the same engine processes within 2 s. A launch whose agent died during
weight loading settled at its deadline; an uncertain launch accepted operator stop
and cleaned up once the agent returned; and a server restart during uncertainty
adopted the launch and settled it automatically. The run found three product bugs,
each fixed with a regression test: a 12 ms host clock lead made publication
refuse every inventory and made the controller drop every host result (now
admitted within a 500 ms lead and recorded on the controller clock), and a
terminated launch's ingress entry blocked a same-generation retry (now retired on
proven termination). Status still misleads in two cases: `ready` with dispatch
disabled after the engine died, and `stopped` while an Initialize is in flight
with its agent down. Also open: `join host` fails with a relative `--join-file`,
and there is no deployment-level Initialize deadline (the CLI fixes 900 s).

Remote vLLM (matrix gap G01) is implemented locally: the host agent now selects
the adapter by engine and reuses the S1 vLLM plan builder, extracted to
`crates/mllm-adapters/src/vllm/frozen.rs`. Affected-crate tests (507), the core
suite and Clippy pass. No live remote vLLM run has happened yet.
The five binary role startup/enrollment/
reconnect tests also pass. A targeted recovery regression confirms that replayed
historical native evidence retains ownership but does not refresh readiness or
reopen ingress. Full restart readiness recovery remains required for U8. U5 still
needs the complete remote inference lifecycle and native host-a gate; it is not done.
The final affected integration run passes 178 tests after adding session-loss gate
closure. Ready publication and disconnect share a lock; a threaded race regression
proves that late completion cannot reopen a disconnected session's gate. Reconnect
preserves ownership but does not promote a historical probe to fresh readiness.
The server also refuses forwarding for revoked enrolled hosts. Agent/controller
and Store Clippy remain clean. These checks do not qualify native execution.

U1 preserves local ledger keys and immutable
receipts; group reservation and remote execution remain separate pending work.
The requested end-to-end two-Spark inference/recovery/cleanup test is distinct
from the broader F4 residency, switching and cache qualification. Those
capabilities remain unqualified until their own evidence is complete.
Plan review covered coherence, feasibility, scope, security and adversarial
assumptions; it corrected an overbroad completion condition that had made all
F4 cache and switching work a prerequisite for this task.

Read-only SHA-256 comparison on both Sparks confirms identical checkpoint
configuration, weight index, all three safetensors shards and tokenizer files
under `~/models/qwen3-4b-instruct`. No model or engine was launched for that check.
Both Sparks report 200 Gb/s on their two direct interfaces. Bidirectional ICMP
on `192.0.2.10`/`192.0.2.11` succeeds; this is connectivity evidence,
not measured throughput or NCCL qualification.
Enrollment certificate primitives now reject forged/malformed requests,
strip requested CA/server privileges, and preserve the host's public key;
four focused tests and agent Clippy pass. This alone does not establish
completed enrollment or remote transport.
Protected atomic identity storage now passes seven focused tests and Clippy.
It preserves existing files, rejects unsafe/partial state, holds an exclusive
local lock, and permits only one concurrent replacement of a given identity
revision. Typed enrollment persistence, the additive v17 registry and TLS/API
integration now pass U2 verification: 645 distinct core tests, 24 agent/protocol
tests, then all 51 management tests after its JSON error-envelope correction.
Core and affected-crate Clippy pass with warnings denied. Tests prove exact
enrollment and renewal replay, concurrent redemption, expiry and hostname
collision denial, zero bootstrap RPCs to an untrusted server, denial of a trusted
but unregistered certificate, and revocation rejection on an existing TLS
connection. U4 must still close actual AgentControl streams on revocation/expiry
and reauthorize commands. These transport tests do not qualify native multi-node
operation.

U3's canonical typed command digest binds every identity field and action,
normalizes group member ordering, and revalidates typed shape. Five focused
protocol execution tests pass. U3 now adds durable acceptance, session and
assignment fencing, permanent replay tombstones, gated process creation and
owned-process cleanup. Fourteen journal tests cover lost acknowledgements,
restart, cross-journal ticket rejection, missing databases, delayed deadlines
and retained uncertainty. Integration passes 646 distinct core tests and 100
reported agent/launcher/protocol tests (including a nested child summary);
Clippy passes with warnings denied. A reused process-group leader can no longer
be mistaken for verified cleanup. Actual authenticated session wiring and
resource/profile authorization remain U4/U5. These CPU and controlled-child
checks do not qualify native multi-node SGLang.

U4 now exposes strict server/host configuration, atomic role initialization,
private invitation files, join, host inventory views and foreground role startup.
The shipped binary test starts a GPU-free server, enrolls an unprepared host,
reports it online but ineligible, and restarts both roles without changing host
identity. Five product role tests pass, including competing initialization and
explicit missing-config denial. Real TLS session tests cover claimed-identity
mismatch, session replacement, revocation, certificate expiry and bounded queues.
Full integration passes 648 distinct core tests; config/agent/protocol tests pass
128 checks. CLI library/grammar tests and core/CLI Clippy pass with warnings denied.
Host full engine logs require the local `--debug-engine-logs` flag, default off.
U4 rejects execution until U5 supplies approved local resource/profile authority;
received history summaries alone cannot settle ownership or readiness. No native
remote or multi-node qualification is claimed.

## Prior SGLang 0.5.19 gate failure — 2026-09-21

Launch/configuration fixes are committed as `047007a`; the scoped runner and
failure-evidence retention are committed as `5d07f11`. Unrelated working-tree
changes remain uncommitted. The subsequent live run used that working tree.

The authorized host-a run reached the protected wrapper, then failed with
`source_revalidation_failed` in 4.43 seconds. The selected SGLang 0.5.19
installation has group-writable package files/directories, and seven of ten
audited source files disagree with the recipe's pinned hashes. The gate remains
closed. At that point environment changes were prohibited. The owner subsequently
authorized the 0.5.20 migration described above. That earlier attempt changed
no installation or driver and rebooted no host.

The deployment settled stopped with admission and dispatch disabled; no engine
processes remained in the post-run check. Evidence is under
`target/live/20260921T213446Z/`, with details in `spark-live-f2.md`. Private state
is retained on host-a at `$HOME/.tmphQyl3M`. This is not native
qualification. Final S3 review and S2 remain pending behind the live gate.

## Local SGLang launch validation — 2026-09-21

At HEAD `b9b33af`, the uncommitted launch fixes pass the local diagnostic:
standalone reaches the wrapper and reports `launch_failed`, with journal evidence
`sglang_startup_failed: artifact_mismatch`, using `/usr/bin/python3` and the stub
checkpoint. This replaces the prior never-armed failure in this local reproduction;
it does not demonstrate native model readiness. The diagnostic state is retained
at `$HOME/.tmpuCTuz5` (machine-local). Running it inside the sandbox
first failed controller ownership checks because sandbox ancestor UIDs appeared as
`nobody`; the successful run used real host filesystem ownership without weakening
the checks.

The required five-crate core command passes 634 distinct tests (635 reported,
including the owned-state child-process duplicate). Configuration tests pass;
the `roles_f1` and `standalone_lifecycle` CLI targets pass 5/5 with one test thread.
All-target Clippy passes with warnings denied for the five core crates. These are
CPU/Fake checks, not native qualification. The excluded owner files were not read,
edited, formatted, tested or staged. No commit or live host run was performed.

The two configuration concerns are fixed in the working tree. The pinned SGLang
recipe rejects `trust_remote_code: true` during configuration normalization, even
when the host security switch permits remote code. Omitted host `deep_park`
policy now means disabled, and standalone requires `MLLM_DEEP_PARK=on` to enable
it; missing, empty, `off`, and unrecognized values do not grant permission.
SPEC §9.1 and T21 now reflect the current working agreement rather than the older
default-on decision. The SGLang standalone template requests deep residency, so
its next native run requires explicit opt-in. The local diagnostic above predates
this default change and was not rerun in this session.

Focused configuration tests, 14 standalone configuration unit tests, and the five
CLI lifecycle tests pass. Regression tests prove default denial, explicit opt-in,
and early rejection of unsupported remote code. The controller launch regression
also verifies omitted policy renders no sleep flags and sets
`VLLM_SERVER_DEV_MODE=0`. Core and affected-library Clippy pass with warnings
denied. The checkout contains broad pre-existing changes, including formatting,
beyond these fixes; they remain uncommitted and must not be bundled blindly.
The final core run passes all 634 distinct tests (635 reported). An earlier run
hit `AddrInUse` in the SGLang stub-engine test; the full isolated retry passed.
The sandboxed attempt was blocked by home-directory ownership and write checks,
so integration verification used the real host filesystem.
Native rerun and the final S3 review remain pending.

Current host authorization comes from the working agreement: both host-a and
host-b are authorized. The owner authorized the SGLang 0.5.20 environment
migration on host-a; driver changes, reboots, and unrelated environment
changes remain prohibited. Older authorization and closed-entrypoint statements below are
historical and do not override that agreement or the S3 composed startup gate.

## Recent committed work

- `dc5a3c1`: a success resets the attempt budget.
- `0a0a3ee`: retry a failed deployment three times with a 30 s doubling cooldown,
  added through `CoordinatorOptions`.
- `49f8bde`: the ordinary path's types, methods, operation kind and event kinds
  lose the qualified prefix.
- `0556375`: the Fake engine's lifecycle simulation renamed under its own name
  (`fake/lifecycle.rs`, `FakeFault`).
- `9530811`: retired `qualification_id` — the host YAML key, profile field and
  token, and bindings renamed to `identity_id` and `recipe_fingerprint`.
- `92fdeee`: deleted the store/config/domain qualification modules; schema v13
  drops ten tables and removes the negative identity guards.
- `9c8f68a`: deleted the candidate lanes, management routes and the CLI qualify
  verb.
- `1f13d4f`: a closed deployment offers no work; a planned step still expires.
- `f42e84d`: an ordinary test fixture that never runs a candidate suite
  (`tests/support/fixture.rs`).
- `bf4b042`: `NativeLaunchHandoff` is sourced through a `NativeLaunchSource`
  trait.
- `6b2e073`: moved `ArmResult`, cleanup types, SGLang pins and native launch
  types out of the candidate module.
- `57247ae`: the domain park contract as pure rules (`mllm-domain/src/park.rs`).
- `df19a46`: qualification is not an mllm concept (ADR 0011 decision 2; SPEC
  §8.4 withdrawn).
- `1761f07`: a failed deployment closes its own admission, not the host's.
- `c6915fd`: the A1 gate on the Fake engine — deploy, start, and one inference
  served through the router, with the coordinator as the sole authority.
- `d2a6117`: a start binding is identified by what it is, not by one spelling, so
  a restart-only deployment can be started at all.
- `8c9a17a`: associated candidate Cleanup through the original retained runtime,
  with clock-free discovery and verified atomic release.
- `ec05bd8`: owned candidate Abort with retained accounting and strict SSE replay.
- `c6da962`: deadline-bound owned Finish with preserved V3 catalog history.
- `8a1fbec`: owned candidate Park/Restore through the original retained Fake.
- `67c0678`: authenticated candidate-run creation using the owned Store/session,
  shared bounded command capacity, exact durable retries, and no runtime effects.
- `1612945`: ordinary owned cleanup acceptance, arming and verified completion;
  generation fencing and atomic release only after exact cleanup evidence.
- `153f6d9`: application-owned bounded local pressure monitor with independent
  stale-read watchdog and cancellation-safe shutdown.
- `744d542`: pinned detokenizer source added to startup verification; all ten
  selected files match the isolated installation and upstream pin without imports.
- `2c2b491`: bounded coherent historical operation lookup.
- `8f238c9`: bounded F2C latency summaries with separate failures and timeouts.
- `d430074`: owned ordinary Fake cleanup through verified durable release.
- `2aff45e`: scoped Start receipts preserved across cleanup and replacement.
- `949609b`: versioned private native launch scope from persisted execution.
- `ae919db`: isolated Python 3.12.3 decoder verification on host-a.
- `7d139a8`: owned Start admission serialized with shutdown and fatal closure.
- `9de1fa8`: no-site isolated interpreter startup before protected native guards.
- `14c2923`: authenticated owned Start and Stop submission.
- `f3a2684`: bounded exact-marker correctness validation for the future F2C runner.
- `3e4bd89`: expired never-armed Initialize terminalization without runtime effects.
- `e4e2e95`: separate monotonic request timing validation.
- `f40abd1`: explicit Stop for never-armed Initialize without runtime cleanup.
- `c0a07dd`: bounded metadata-only request journals.
- `e11c3de`: private descriptor-relative artifact storage with a shared byte cap.
- `861a3ad`: bounded collected JSON marker response validation.
- `eb8e572`: bounded streamed marker data-event validation.

Expired, never-armed ordinary Fake Initialize requests now terminalize atomically.
The worker proves absence of execution, grants, ownership and runtime identities
before releasing unused endpoint and binding reservations. No memory release,
cleanup evidence or ledger epoch is invented. Armed uncertainty remains retained.
Expiry events replay through the management SSE stream.

Explicit Stop before Initialize arms also passes root integration. Acceptance
fences generation and retains reservations; the owned worker releases only after
the prior task exits and a separate atomic no-effect proof succeeds. It records
distinct Stop history and SSE events without cleanup evidence or a memory epoch.
Associated runtime cleanup remains unchanged. Armed unassociated work is retained.

F2C request journals now serialize only closed outcome codes, numeric corpus
ordinals and validated monotonic durations. Private artifact storage creates
exclusive mode-0700 run directories and mode-0600 fixed files relative to a trusted
parent descriptor. Writers share an at-most100-MiB payload cap and stop on failure;
partial evidence is retained. Each file requires explicit sync. These helpers do
not validate a run manifest, prove route identity, authorize effects or complete
the API-driven runner.

Collected JSON and decoded streaming data events now have bounded marker checks.
They validate the served model, one choice, exact ordered content and natural stop;
streaming also requires one terminal event. Malformed envelopes and alternative
output fail closed without response text in diagnostics. Streaming checks accept
already-decoded data events, not raw SSE bytes, and reject usage-only events.
A bounded LF/CRLF framing helper now feeds those events across arbitrary network
byte splits, including split UTF-8. It supports comments and multiline data but
rejects other SSE fields, lone CR and incomplete frames. HTTP status/content type,
clean transport completion and binding provenance remain runner obligations.

The F2C phase-margin calculation now uses checked integer arithmetic for
`peak + max(2 GiB, ceil(peak/4))`. It rejects overflow instead of saturating or
wrapping. This numerical helper does not establish attribution, verify a recipe
or reduce any reservation; missing attribution keeps the conservative grant.
Its pressure-case helper selects the smallest whole-GiB ceiling covering every
supplied intermediate charged demand while denying direct wake. It rejects an
empty or unsafe interval. Actual verified attribution, complete ledger totals
and planner feasibility remain caller obligations, not numerical assumptions.

The candidate pipeline these paragraphs used to describe in detail — Initialize,
run-scoped inference, Park/Restore, Finish, Abort, Cleanup, and the qualification
ceremony that gated them — is deleted (ADR 0011; Tasks 7–9 of the qualification-
removal plan, commits `9c8f68a`, `92fdeee`). Narrating its internal mechanics here
would describe code that no longer exists; see "Recent committed work" above for
the deletion commits and the A1b entry below for what replaced it.

Authenticated Start and Stop HTTP submission now passes root integration
verification. The optional lifecycle router shares the existing owned state,
trusted principal and bounded command capacity. Stop resolves generation in its
acceptance transaction after historical receipt lookup. Exact retries survive
cleanup, replacement and worker shutdown; accepted responses do not claim Ready
or cleanup completion. Narrower routers gain no lifecycle authority. No listener,
native lifecycle support or additional cleanup/recovery path is introduced.

Scoped Start command receipts now pass root integration verification. Exact
retries preserve the original operation, joined value and deadline after Ready,
verified cleanup, replacement and valid session rotation. Historical reads grant
no execution authority; current resource-policy gates still govern new acceptance.
The owned worker now provides bounded command handles. Exact receipt history is
read before current admission flags; fresh commands serialize with shutdown,
Drop, initialization pause and fatal closure. The HTTP adapter maps typed Store
errors to fixed public categories. Retained handles keep the owned state/process lock
for historical reads but cannot restart execution.

The owned Fake cleanup worker passes root integration verification. It retains
the original instance, waits for Initialize to exit,
supports explicit same-session cleanup after associated uncertainty, and sends
only after a new durable cleanup arm. Unverified outcomes retain authority.

## Remaining implementation and verification

1. Complete ordinary warm lifecycle, sequence/preinitialization, no-spawn
   terminalization, missing-association cleanup and restart reconciliation.
   Expiry and explicit Stop for never-armed Initialize are implemented. Other
   no-spawn states and missing-association/restart recovery remain open.
2. Ordinary park (drain, park, parked accounting, wake) is not designed; the
   contract it must satisfy is `mllm-domain/src/park.rs`. The ordinary native
   launch is designed and its launch path is landing on this branch;
   `NativeLaunchHandoff` waits on a `NativeLaunchSource` implementation.
   Native parking is blocked on both engines regardless: `VllmAdapter` has no
   `execute_persisted`.

## Milestones and review gates

Work follows [ADR 0009](../design/adr/0009-proof-carrying-reconciliation.md). Review
happens at a milestone boundary, not after each task. Units inside a milestone are
verified by focused TDD plus the core suite and carry no separate review pass.

Each milestone leaves a working system and is independently reversible. The order is
deliberate: the largest deletion is last, because doing it first would restructure the
most intricate logic in the project against tests that have never run in production.

### A1 — Production cutover

The cutover is done on the Fake engine and the gate is met there. A native engine
still cannot be started, for the reason recorded below.

- [x] Engine family to adapter resolution (`mllm-adapters/src/resolve.rs`).
- [x] Proof that recorded processes are gone, from identities rather than a live
      handle, so it survives the restart that destroys handles.
- [x] Engine-generic driver factory (`spawn_resolved`): read the declared engine,
      build an `AdapterSpec`, resolve, prove cleanup with `observed_gone`.
- [x] Wire the coordinator into `roles.rs`; retire the handle map; resolve adapters
      per binding (`23e3f35`, `ffe6af6`, `8996065`).
- [x] Accept an ordinary Start for a restart-only deployment (`d2a6117`). The start
      validator asserted the fake-engine fixture's shape, so every Start was refused
      as corrupt stored data and nothing could run at all.
- [x] Drive an accepted Start to Ready and serve through the router (`c6915fd`).
      Four fixture assumptions blocked it: the observation source reported the
      agent's `system` label rather than the host's declared domains; `AdapterSpec::Fake`
      resolved to a bare `FakeEngine` whose `execute_persisted` answers `Unsupported`;
      the Fake engine recognised an ordinary initialize by a `qualified:` id prefix;
      and the router read routes only from the legacy `route_model_id` column, which
      managed configuration clears.
- [ ] Remove the legacy authorities together, as the A2d plan requires: synthetic
      admission, empty-ledger checks, old reservation writers, router-owned eviction
      and in-memory release guards. Never two authorities at once.

**Gate:** deploy, start and serve one inference through the router with the
coordinator as the sole lifecycle authority, on a real engine.

`crates/mllm-cli/tests/a1_gate.rs` is that gate as one test and it passes **on the
embedded Fake engine**, which is what standalone declares when no live profile is
configured. That is the first end-to-end evidence the project has, and it is not
verification of a native recipe (SPEC §18).

The native half of the gate is still open. The owner confirmed on 2026-09-16 that
**both** engines are required, not one:

- vLLM is done: `VllmAdapter::execute_persisted` launches from the frozen profile,
  records process identities and probes readiness, and it is live-green (S1, run 6
  below).
- SGLang: the native entrypoint denial was **composed open** on 2026-09-19
  (owner-authorized; S3 plan
  `docs/superpowers/plans/2026-09-19-sglang-launch.md`, commits `393b197..17b6ffc`).
  `sglang_entry._verified_native_contract` runs the audited gates (source
  revalidation, plugin closure, placement, checkpoint) and the guarded engine
  import follows when the contract holds; the ordinary descriptor carries
  `sglang_private_launch`/`sglang_launch` kinds and the route-name served token.
  The mllm side is complete: `ProfileBindings` builds the SGLang runtime, seals
  inference + admin roles (`engine_secrets` v15), the adapter spawns through
  protected descriptors (v2 launch scope), readiness is real, and the Rust and
  Python validators are ASCII-printable-parity. **Not yet live-verified:** a live
  launch still needs the host to publish `device_inventory_digest` and the
  guarded launcher to set the child's `CUDA_VISIBLE_DEVICES`; until then the
  audited argument mapper fails closed (`placement_mismatch`). Residual
  placement risk: any single GPU in the verified inventory satisfies placement,
  because `device_id` is not yet bound to a physical UUID
  (`DevicePolicy.physical_gpu_uuid` is the hardening follow-up). Task 10 of the
  plan is the live gate on host-a; nothing here qualifies the native recipe.

Both are read from the code, not from an observed run: no native SGLang start has
been attempted since the cutover.

Park was originally part of this gate and has moved to A1b. The ordinary lifecycle
has no park at all; the candidate path that formerly had one is deleted (ADR 0011).
Ordinary stop returned with `b52f729`, which split the suspension predicate;
park has not.

The owner confirmed on 2026-09-16 that parking is the product's premise, not an
option: **one model parked while another serves, switching between them
automatically, is the reason the box holds more than one model.** Anything that
reduces eviction to stop-and-restart misses the point of the project.

### A1b — Implement eviction in the authority

Pressure-driven switching is not implemented in the product. `SwitchEngine` is the
only implementation of drain-release-wake, it lives in the router, and production
never constructs it: `mllm-cli/src/roles.rs` wires `WakeJoin` and `auto_activate`
instead. Before the port extraction, `ready_deployments_excluding` had exactly one
caller, `switch.rs`. `request_transition_inner` handles suspension flags and the
preinitialize contract and never looks at another deployment.

So when a request arrives for a deployment while another holds the exclusive pool,
nothing releases the incumbent. The engines can perform the switch — that was
measured on 2026-09-16 — but mllm has no way to ask for it. This is why the F2 exit
gate's warm-switching criterion could only be demonstrated engine-direct.

- [ ] Implement drain, release and wake in the lifecycle authority, taking
      `SwitchEngine`'s semantics as the contract: close admission, bounded drain
      grace, quiescence through the adapter, park or stop by declared tier, and on
      failure reopen the incumbent unsuspended and journal the failed switch.
- [ ] Move the activation join to the authority so simultaneous arrivals collapse to
      one operation, keyed by deployment, revision and generation.
- [ ] Delete `SwitchEngine` and, with it, the two writes the router currently makes
      through the port.
- [x] Give the ordinary stop an intent (`b52f729`). Schema v11 adds `admin_stopped`,
      carrying the operator's intent alone; `suspended` keeps its nine eligibility
      readers untouched. This is what the earlier attempt could not do by writing
      `suspended`, from either side of acceptance.
- [x] Make the park tier declarable and host-validated (ADR 0010). Residency names
      the tier (`restart_only`, `host_backed`, `deep`); a host declares each domain's
      memory topology; a host-backed park is refused at configuration time on a
      one-pool domain; SGLang's startup flags follow the declared tier. This makes
      the choice expressible and checkable. It does not implement park.
- [x] Remove qualification (ADR 0011). mllm guards the host; the user owns the
      recipe. The candidate and qualification subsystem is deleted, schema v13
      drops its tables, the park contract survives as pure domain rules, a
      failed deployment closes its own admission and is retried three times
      with a doubling cooldown before it is given up on, within the start
      command's deadline, and an uncertain attempt still resolves through the
      gone-proof first — an explicit Stop drives that cleanup and the Start
      that follows is a new generation with a fresh budget. CPU and Fake tests
      are not verification of any native recipe.
- [x] S1 — native launch (vLLM), live-green on host-a on 2026-09-18 (run 5
      at `000b832`, six of six scenarios, evidence entry in
      `docs/runbooks/spark-live-f2.md`). The coordinator directs a native
      builder to launch a real vLLM 0.29 engine: cold start to Ready in 27 s,
      inference through the router, loopback-only listening with the guard
      middleware refusing unkeyed control routes, a stop that proves the group
      gone in 1.2 s, a restart under a new incarnation, a bad model source
      closed in 5 s with the next start on the same controller reaching Ready,
      an executable that exits at once closed with no leftovers, and memory
      returning after stop. Landed on the way: encrypted per-launch engine keys;
      a launch that fails after arm is terminated, proven gone and released with
      evidence; configuration for `deep_park`, the model store and the model
      source; the Fake engine moved out of the product into `mllm-testkit` as a
      test fixture; the router's per-deployment forwarder keyed on what the
      coordinator recorded. Runs 1 to 4 each found a defect the CPU suite could
      not see because its fixtures did not have the launch path's real shape
      (pre-flight self-match, api-only identity refused as corrupt, adapter
      probing without its key, zero start ticks on this kernel failing every
      process scan, re-admission gap after a closure); each is recorded with
      its fix in the live runbook. Open items: post-launch retry stays deferred
      to SPEC §6; state directories written before commit `e4dcd20` must be
      recreated, because the model-source shape changed the recipe fingerprint;
      vLLM 0.29 authenticates only `/v1`, `/v2`, `/inference` and `/cohere`, so
      `runtime/mllm_vllm_guard.py` covers the remaining development routes
      itself and L3 holds that true; the manifest-hash fingerprint for a
      `local` model source is deferred to S1b, so standalone still writes the
      placeholder `sha256:<name>` and the spec is amended to say so; whether a
      parking residency with `enable_sleep_mode` false and `deep_park` enabled
      should be refused or defined as restart-only parking is a question for
      the S2 ADR, since such a profile resolves today and S2's park would call
      `/sleep` on an engine started without `--enable-sleep-mode`; S1r (restart
      re-attach) is next. What
      this establishes is vLLM 0.29 with qwen3-4b-instruct on this host and
      nothing about parking, SGLang, re-attach or other builds. CPU and
      Fake-engine tests here are a pre-check, never the claim that a native
      engine recipe works live. Confirmed again after the whole-branch
      review's fix wave (`a7e72ff`..`44a3a42`) and the three re-review items
      (`74aa941`): run 6 on 2026-09-19, six of six, 137 s, no leftovers
      (evidence under `target/live/20260919T152154Z/`).
- [ ] Implement ordinary park. The ordinary lifecycle has no park at all; the
      candidate path that formerly had one is deleted. This is the premise of
      the product and the largest remaining piece of A1b.

Ported faithfully first, keeping the existing T16 and T19 tests as the contract. The
semantics were written against F1's assumptions and deserve revisiting against the
proof-carrying model, but that belongs in A3 rather than here, where it would rewrite
the tests that define correct behaviour.

**Gate:** a request for a deployment whose pool is held by another causes the
authority to release the incumbent and serve the request, with no router involvement
beyond asking.

### A2 — Extract the domain

`mllm-store` is larger than the controller, management, adapters, router and
scheduler combined because workflow logic followed the transaction into it.

- [ ] Create `mllm-domain` as a pure crate: planner, policies, resource algebra,
      proof rules. No async runtime, no clock, no engine knowledge.
- [ ] Move rules out of `lifecycle`, `progression`, `initialize`, `security`, `warm`
      and `cleanup` with no behaviour change. The store keeps its tables.

**Gate:** `mllm-domain` compiles without an async runtime and its tests run with no
database and no network. A rule that cannot be tested that way is in the wrong layer.

### A3 — Capabilities and proofs as data

- [ ] Engines declare the actions they perform and the facts they can prove instead
      of failing when called.
- [ ] The domain permits a transition when its required proofs are a subset of what
      the installation proves.
- [ ] vLLM reaches restart-only parking by mechanism rather than by special case,
      which is the fallback `SPEC.md` §6.2 already describes.

**Gate:** adding an engine family requires publishing two sets and no coordinator
edit. The proof set gates the commit, not the call.

### A4 — Collapse the second lifecycle

Discharged by deletion (ADR 0011).

## Tracked for later: naming and engine resolution

[ADR 0008](../design/adr/0008-engine-installations-and-runtime-types.md) makes
"engine installation" the term of record, but internal type names still say runtime
profile. Rename `RuntimeProfile` and its configuration key, and keep one name per
concept on every new surface in the meantime. The mockups additionally use "runtime"
for three different things — start mechanism, Python version and CUDA version — and
only the first is the runtime type; the others are build metadata that
`build_fingerprint` already covers.

Add the engine-family to adapter resolution layer. Adapters are currently selected
at hardcoded construction sites, which is what blocks both a third engine family and
the construction of `SglangAdapter` on the runtime-binding path.

Two mockup behaviours conflict with the spec and should not be implemented as drawn.
Raw engine flags include `--served-model-name`, which `engine_policy.rs` reserves,
and the interface warns that a raw flag overrides a structured setting; T14 requires
conflicts to fail with provenance instead. The CLI grammar is also resource-first
(`mllm hosts list`), where R11 requires action-first (`mllm list hosts`).

## Earlier multi-node constraints, updated for the current two-Spark work

These constraints were originally deferred beyond the standalone F2 recipe.
The owner's current two-Spark instruction and the plan linked above now govern
this work. The standalone recipe remains TP=1, DP=1; distributed qualification
requires its own recipe and evidence under SPEC §11.

1. Local completion now admits one `api` plus contiguous `worker-0..N` identities
   sharing a boot identity. U1 adds a separate host/member-scoped group contract,
   so equal PIDs on different hosts are valid. Group lifecycle settlement is
   still pending; the local completion path does not establish it.
2. General tensor, pipeline, data and expert layouts remain unqualified. U1's
   group contract permits only two distinct hosts with one device each and ranks
   0/1. U7 still needs to connect that topology to configuration normalization
   and the distributed native recipe under T27.
3. U1 adds `mllm-domain::group` with member identities, TP2 ranks, peer addresses
   and rendezvous data. Production group reservation, dispatch and recovery
   remain pending under U6; validated shape alone grants no launch authority.
4. Multi-rank release and resume acknowledgement is unverified. Per the F2B plan,
   SGLang's release and resume await their communicators, and a success reply from
   the tokenizer manager does not prove every rank released. A partial release that
   reads as success would be exactly the unevidenced release `SPEC.md` §6.1 forbids.
   The 2026-09-16 verification proved the single-rank path only.
5. `NativeResidencyObserver` now fails closed when allocations span more than one
   device, because summing mapped bytes across devices cannot distinguish a fully
   restored group from one restored rank. Per-rank evidence, and cross-host
   aggregation for a multi-node group, remain unimplemented.
6. Hardware: host-a has a single GB10, so no parallel topology can be verified
   there. Tensor parallelism needs a multi-device host; multi-node needs two hosts.

## Open questions

1. A caller waits the full 600s bound to learn an operation is uncertain.
   When `drive` fails after arming, the worker marks the lifecycle run `uncertain`
   and pauses, holding the retained binding until an explicit Stop and a verified
   cleanup. That is the proof-carrying model working as intended. But the
   `operations` row stays `running`, and `CoordinatorLifecycle::classify` reads only
   that row, so `wait_terminal` polls for the whole `TERMINAL_WAIT` (600s) before
   reporting `Uncertain`. Observed on 2026-09-16: a regression run took 600.15s to
   fail. The run state is durable and says `uncertain` immediately, so the caller
   could be told at once. Changing it alters what the router does with a request
   that triggers activation — it would fail fast rather than hold the client — so it
   is the owner's call, not a repair to make unattended.

2. Stopping a deployment that was never started reports a conflict.
   `accept_ordinary_cleanup_in_transaction` finds no unreleased runtime binding and
   returns `LifecycleError::Conflict`, which reaches the caller as
   `LifecycleFault::Conflict` — "your view is stale, re-read and retry". Re-reading
   will not help: nothing was ever started, so this is an illegal transition and the
   honest answer is a refusal. `LifecycleError` has no variant for that today;
   `Disabled` is the closest and means something else. Pinned by
   `standalone_lifecycle::stop_is_illegal_from_stopped` so a change is deliberate.

3. A resource policy written before ADR 0010 cannot be read back.
   `StoredPolicy.version` stayed at `1` when `StoredDomain` gained a required
   `memory` field, so a policy row written before that change now fails to decode as
   `CorruptStoredPolicy`, and `import_resource_policy` does not overwrite it — it
   reads the existing row first and propagates the error. Failing closed is correct:
   the old row genuinely lacks the topology fact and ADR 0010 forbids inferring it.
   The defect is the diagnosis, which says "corrupt" for what is merely a superseded
   shape. No persisted policy exists on this machine. **If standalone refuses to boot
   against a state directory created before 2026-09-16, delete the directory** — the
   host policy is republished at every boot.

4. Retry cooldown sleeps inside the single worker loop. When a deployment fails
   and is waiting out its doubling cooldown before the next attempt, the worker
   sleeps in place, which blocks it from discovering and advancing any other
   deployment for up to 30 s per wait. This is accepted for A1b standalone,
   where one worker and one deployment are the common case, but it will not
   scale past that. The fix shape is a not-before time read from
   `deployment_attempts.last_attempt_ms` and checked at poll time instead of a
   blocking sleep, so the worker keeps discovering other deployments while one
   waits out its cooldown.

5. The give-up reason is recorded in the journal but not in the deployment's
   own state. Every counted attempt and the give-up itself now write a
   `journal_entries` row naming the deployment and the reason, in the same
   owned transaction that counts the attempt or closes the admission, and the
   observer reports a planned step of a closed deployment as `Closed` rather
   than `Superseded`. What is still missing is a failure category or
   last-error text on the deployment row itself, so a caller reading only
   `deployments` still cannot tell a budget exhaustion from any other reason
   admission might be closed.

6. Stored kind strings of the ordinary path were renamed in the same change
   without a data migration; a v12 state directory that holds lifecycle
   history is recreated, as the design keeps no compatibility. `operations.kind`
   went from `qualified_initialize` to `initialize`, the owned-launch
   association tag from `qualified_owned_launch` to `owned_launch`, the
   management event kinds from `qualified_*` to `initialize_*`, and the plan's
   own tag with them. Schema v13 drops tables and rewrites none of these, so a
   pre-v13 directory carrying lifecycle rows fails to decode rather than
   upgrading. **Delete such a directory**; the host policy is republished at
   every boot.

7. Fingerprint drift between an effective configuration snapshot and the
   current host is not checked at deployment start. The refusals that used to
   catch a stale or mismatched recipe came from the deleted qualification
   catalog and judged the recipe, not host capacity; nothing replaced that
   check when the catalog was removed, so a start can proceed against a
   snapshot that no longer matches the host it targets.

7. The dispatch seam — grant, close, finish and pending dispatch, and
   `request_leases` — has no production issuer. Router dispatch ownership (the
   F2A2c plan) is the intended one and has not landed. Ordinary tests use the
   seam directly today to exercise the request-lease guards, which is useful
   coverage but not evidence that anything in production calls it.

8. Two SGLang wire kinds, `sglang_launch` (public) and `sglang_private_launch`
   (schema version 2 private descriptor), plus the served-name rule "the
   deployment's route name" (`work.effective().routes.first()`), are the
   contract the 2026-09-19 ordinary native launch rename produced. The former
   `sglang_candidate_*` kinds and the `candidate-{binding_id}` rule are gone;
   both validators reject them.

9. `crates/mllm-controller/src/sequence.rs`'s planner still keeps a
   `qualified_park`/`qualified_restore`/`qualified_initialize` eligibility
   vocabulary inherited from the legacy F1 `Controller` lineage. This plan did
   not touch it; renaming it is work for the park ADR that wires ordinary park
   against the `mllm-domain/src/park.rs` contract. The legacy
   `crates/mllm-controller/src/operations.rs` `Controller` itself is untouched
   by this plan and remains slated for retirement at the A2d gate, per the
   milestones section above.

## Owner attention

Two items from S1, 2026-09-18. `crates/mllm-cli/tests/live_interactive.rs`
(the owner's, excluded from agent edits) uses `ParkPolicy::ExperimentalAllowed`,
which is now a deprecated alias of `ParkPolicy::Enabled`; workspace-wide clippy
with warnings denied fails on that one line, and every other crate passes. The
alias constants and the `start_standalone_with_policy` and
`LiveVllmProfile::from_env` shims exist only for that file and can go once it is
updated. Its test `lab_http_auth_status_and_busy_controls` also fails, because
it boots standalone without declaring an engine installation, which S1 made a
refusal (`NoEngineInstallation`). Separately, `roles_f1.rs` is flaky under
parallel test threads; this predates S1 and the live runner uses one thread.


Execution capacity item: after Cleanup committed, fresh-worker creation for the
queued trusted response-capture unit failed with `agent thread limit reached`.
The visible workers are complete and no Cargo process remains. The available
tools expose no worker close/release operation. The current execution skill keeps
capacity-limited work queued and forbids reusing a completed worker for another
unit. Continue from a fresh session with worker capacity, or explicitly direct
inline implementation. No response-capture worker launched or changed source.

Root verification for the associated candidate Cleanup slice: Store 204,
controller 243 and management 67 tests pass (514 distinct core tests). The same
full five-crate run also passes all 88 adapter and 74 harness tests, for 676
distinct tests. Initial integration exposed an unnecessary clock sample during
idle Cleanup discovery. The narrow repair preserves every action clock fence and
the existing two-sample assertion; the complete rerun passes on final code. Two existing
caller-timeout tests failed during the earlier
unarmed Stop worker's concurrent fixture runs, then passed unchanged in isolated
reruns and subsequent bounded full core runs. No timeout or evidence-freshness
limit was changed.
Tests used four threads to bound concurrent fixture load; internal race tests
remain enabled. Separate no-site verification passed 10 renderer, 15 runtime-binding
and 218 Python runtime tests; all16 launch-decoder tests also pass on isolated Spark
Python 3.12.3.
The full root run also passes all 74 harness tests, including five phase-bound/ceiling, six exact-marker,
seven collected JSON, eight streamed-data, five SSE framing, five timing, seven journal and nine
protected-storage tests.
The full Cargo harness count includes the three pressure-ceiling tests.
All-target Clippy also passes for these five crates with warnings denied.
None is native verification evidence.

One existing item remains for the owner's inspection: check the untracked
`crates/mllm-cli/tests/live_interactive.rs` for formatting from the earlier
workspace-formatter incident. There is no original baseline for that file, so
the agent cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified SDD Task 2 report
also remains excluded and untouched by this continuation.

Host access item: RESOLVED. The 2026-09-15 SSH timeouts no longer reproduce.
A read-only check on 2026-09-16 connected successfully; `host-a.tailnet.ts.net`
resolves to `100.64.0.10` over Tailscale and port 22 is open.

Kernel and GPU driver item: OPEN, owner action required. host-a rebooted at
2026-09-15 22:53 into kernel `7.0.0-1019-nvidia`, which has no GPU driver module.
`modprobe -n -v nvidia` reports `FATAL: Module nvidia not found in directory
/lib/modules/7.0.0-1019-nvidia`. No nvidia modules are loaded and no `/dev/nvidia*`
nodes exist, so `nvidia-smi` fails. The previously booted kernel
`6.17.0-1031-nvidia` still carries the complete stack: `nvidia.ko`, `nvidia-uvm.ko`,
`nvidia-drm.ko`, `nvidia-modeset.ko` and `nvidia-peermem.ko`. Driver packages
`nvidia-driver-580-open 580.173.02` remain installed. `GRUB_DEFAULT=0` selects the
newest kernel, so the upgrade silently changed the boot target.

Two owner options. Booting `6.17.0-1031-nvidia` and pinning it is the faster and
more reversible one; building the 580-open driver for `7.0.0-1019-nvidia` through
DKMS is the forward fix. Either way, pin the boot entry so a future kernel upgrade
cannot silently remove GPU access again. The agent did not change drivers, modules,
boot configuration or power state; all checks were read-only.

No new approval is required for the current bounded implementation. Only
host-a is authorized. The approved isolated SGLang environment and reviewed
observer patch do not authorize changing existing engine environments, drivers,
rebooting, or accessing host-b. Both native entrypoint denials remain closed;
there has been no model load or native verification in these slices. Build and
live-effect gates remain explicit rather than inferred from passing CPU tests.
