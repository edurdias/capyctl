# Live evidence: F2 on host-a

Purpose: this is the record of what was actually observed when mllm drove a real
inference engine on real hardware. Nothing else in the repository can stand in for
it — the CPU and Fake-engine suites prove that the controller's machinery holds
together, and passing them establishes nothing about a native recipe (SPEC §18).

Until 2026-09-23 every run of `scripts/live/run-on-spark.sh` appended one entry below,
newest last. An entry names the commit it ran at, so the code that produced a result
can be recovered; it never carries the output itself, which lives in
`target/live/<stamp>/` on the machine that ran the script.

That runner and the suites it ran (`crates/mllm-cli/tests/live_vllm.rs`,
`live_sglang.rs`) drove standalone in process rather than the shipped binary. By owner
decision (2026-09-22) they were replaced by matrix rows driven through the shipped CLI
and roles (M38, M73, M74, M75 in
`docs/plans/2026-09-22-two-host-engine-matrix.md`, harness
`scripts/live/matrix/`) and deleted on 2026-09-23. The entries below are historical and
their commands no longer exist; matrix results are recorded only in
`docs/runbooks/f2-current-status.md`.

Entry template:

```markdown
## 2026-MM-DD — S1 run N — <commit>
vLLM 0.29.0, qwen3-4b-instruct, host-a (GB10, 121 GiB unified). Command: scripts/live/run-on-spark.sh

| Scenario | Result | Timing / sample |
| --- | --- | --- |
| L1 launch | pass | cold start to Ready: NN.N s |
| L2 serve | pass | plain NN.N s, streaming NN.N s |
| L3 access control | pass | engine on 127.0.0.1:NNNN only |
| L4 stop | pass | NN.N s, group empty |
| L5 restart | pass | NN.N s, new incarnation |
| L6 bad source | pass | closed in NN.N s |
| L7 recovery | pass | Ready in NN.N s |
| L8 engine exits at once | pass | closed in NN.N s, no leftovers |
| L9 deadline bound | pass | start refused, nothing launched |
| L10 no engine | pass | NoEngineInstallation; release binary clean |
| L11 memory returns | pass | before NN.NN GiB, after NN.NN GiB |

Failures and what changed: …
vLLM control routes keyed by API key: yes/no (see the auth-scope finding below).
```

## 2026-09-18 — S1 runs 1 to 5 — `87b683c` to `000b832`
vLLM 0.29.0, qwen3-4b-instruct, host-a (GB10, 121 GiB unified, Linux
6.17.0-1031-nvidia). Command: `scripts/live/run-on-spark.sh`. Evidence for run 5 is
under `target/live/20260918T131825Z/` on the machine that ran the script.

Run 5, at `000b832`, is the first green run: six of six scenarios, 138 s wall clock.

| Scenario | Result | Timing / sample |
| --- | --- | --- |
| L1 launch | pass | cold start to Ready: 27.2 s |
| L2 serve | pass | plain 0.1 s, streaming 0.5 s; sample "ready" |
| L3 access control | pass | engine on 127.0.0.1:8100 only; off-host address refused; unkeyed `/sleep` and `/collective_rpc` refused |
| L4 stop | pass | 1.2 s, group empty |
| L5 restart | pass | 27.7 s, new incarnation |
| L6 bad source | pass | closed in 5.2 s; journal names the launch failure |
| L7 recovery | pass | Ready in 26.6 s on the same controller; sample "ready" |
| L8 engine exits at once | pass | closed in 0.6 s, no leftovers |
| L9 deadline bound | pass | start refused, nothing launched |
| L10 no engine | pass | NoEngineInstallation; release binary clean |
| L11 memory returns | pass | before 117.04 GiB, at Ready 89.70 GiB, after stop 117.83 GiB |

Failures and what changed. Runs 1 to 4 each failed, and each failure was a defect
the CPU suite could not have found, because its fixtures did not have the shape
the real launch path has.

- Run 1 (`9dfc383`): the pre-flight matched its own command line through the
  Tailscale SSH wrapper and refused an empty box. Fixed in `87b683c`.
- Run 2 (`87b683c`): four of six hung until the settle bound and left two vLLM
  servers running. Two defects. The store's status observer refused the shape a
  failing launch leaves behind, one API identity recorded and no association, as
  corrupt data, so the worker recorded the failure as unannotated and nothing
  terminated the engine. And the adapter probed `/v1/models` without the key it had
  just given the engine, so a healthy engine answered 401 and a good launch failed
  as uncertain. Fixed in `3103021`.
- Run 3 (`3103021`): settlement now ran but could not prove the group gone:
  "malformed or inconsistent kernel process data". On this kernel, init and every
  kernel thread report a start time of zero in `/proc/[pid]/stat`, and the group
  scan rejected any zero as malformed, so no scan of this host's process table
  could complete. The development machine reports nonzero start times for those
  processes. The retained uncertainty was also invisible to a waiting caller for
  the full ten-minute bound. Fixed in `f7a538e`.
- Run 4 (`f7a538e`): five of six. L7's start, issued the moment L6's closure was
  observable, was refused with "worker is not admitting Initialize": the worker
  re-admitted starts only after its own bookkeeping, and the release transaction
  had already made the closure visible. Re-admission now happens under the lock
  that commits the release. Fixed in `000b832`.

vLLM control routes keyed by API key: no, not by vLLM's own middleware; yes with
`runtime/mllm_vllm_guard.py` loaded, which L3 asserts (see the auth-scope finding
below).

What this run does and does not establish. It establishes that the coordinator
starts, serves through, stops, restarts and fails over a real vLLM 0.29 engine on
this host with this model, and that a failed launch is terminated, proven gone and
released with its key deleted. It does not establish parking (S2), SGLang (S3),
restart re-attach (S1r) or any other model or engine build. CPU and Fake-engine
runs remain no evidence of any of it.

## 2026-09-19 — S1 run 6 — `74aa941`
vLLM 0.29.0, qwen3-4b-instruct, host-a (GB10, 121 GiB unified, Linux
6.17.0-1031-nvidia). Command: `scripts/live/run-on-spark.sh`. Evidence under
`target/live/20260919T152154Z/`.

The first green run after the whole-branch review's fix wave (`a7e72ff`..`44a3a42`)
and the three re-review items in `74aa941` (identity-key read no longer mints a
new key, `/`-leading runs are redacted segment by segment, gate-write reap uses
the `killpg` backstop). Six of six, 137 s wall clock.

| Scenario | Result | Timing / sample |
| --- | --- | --- |
| L1 launch | pass | cold start to Ready: 26.2 s |
| L2 serve | pass | plain 0.1 s, streaming 0.5 s; sample "ready" |
| L3 access control | pass | engine on 127.0.0.1:8100 only; off-host 100.64.0.10 refused; unkeyed control routes refused |
| L4 stop | pass | 1.2 s, group empty |
| L5 restart | pass | 25.6 s, new incarnation |
| L6 bad source | pass | closed in 5.2 s; journal names the launch failure, redacted |
| L7 recovery | pass | Ready in 25.6 s on the same controller; sample "ready" |
| L8 engine exits at once | pass | closed in 0.6 s, no leftovers |
| L9 deadline bound | pass | start refused, nothing launched |
| L10 no engine | pass | NoEngineInstallation; release binary clean |
| L11 memory returns | pass | before 116.83 GiB, at Ready 89.68 GiB, after stop 117.90 GiB |

Failures and what changed: none. This run confirms the review-mandated changes
without introducing a launch-path regression; the numbers are consistent with
run 5 (`000b832`). Redaction is visible in L6's journal tail, where the venv path
and this host's temp directory are blanked before the engine's pydantic error
reaches the journal.

### vLLM auth scope, verified on host-a on 2026-09-17

vLLM 0.29's own authentication middleware guards a fixed list of path prefixes.
From `vllm/entrypoints/serve/middleware/authenticate.py`:

    GUARDED_PREFIX = ("/v1", "/v2", "/inference", "/cohere")

Everything outside that list is unauthenticated, which includes exactly the routes
mllm depends on for deep park: `/sleep`, `/wake_up`, `/is_sleeping` and
`/collective_rpc`. On a development-mode server those are open to any local caller.
SPEC §9.1 and T21 require that surface to be denied by default with explicit opt-in
only, so shipping on vLLM's own middleware would not have met the requirement.

mllm closes the gap with its own ASGI middleware, `runtime/mllm_vllm_guard.py`,
loaded into the engine process through vLLM's `--middleware` flag. It requires a
valid `Authorization: Bearer <VLLM_API_KEY>` on every HTTP and WebSocket path except
`/health`, with `OPTIONS` passed through unchecked because a preflight is not a
control action. The presented token is compared by SHA-256 digest with
`secrets.compare_digest`, mirroring vLLM's own check, so neither length nor timing
leaks anything about the configured key.

Scenario L3 is what holds this true over time: it asserts that an unkeyed `POST` to
`/sleep` and to `/collective_rpc` is refused with 401, and that a `GET /is_sleeping`
carrying the key the launch minted is answered. A regression that dropped the
middleware would leave those routes open and L3 would fail.

### What L3 asserts in place of the forwarder allowlist drive

Spec §9 specifies one more thing for L3: that the per-deployment forwarder refuses
an upstream path outside its chat and models allowlist, driven directly rather than
through the router's route table. The scenario as committed asserts something
different. It sends `/metrics` through the router and reads the 404 the route table
returns, and the argument for that being sufficient is structural: `ChatForward`
carries no path at all, so the only upstream a forwarder can express is the one
`upstream` builds, and a forwarder driven directly has no path argument to refuse.
The allowlist is pinned by a unit test over `upstream` instead
(`crates/mllm-adapters/src/forward.rs`, T19).

That argument is sound, but it is a different proof from the one the spec names, so
the evidence table above should not be read as covering a drive that was never run.
If the forwarder ever gains a path argument, this substitution stops holding and
the scenario has to drive it.

### What run 1 must confirm about L9

L9 was specified as a readiness deadline: a healthy engine still loading when its
deadline arrives is terminated, proven gone, and the deployment closed. Reading the
path shows that shape is not reachable from a deployment document. An administrative
start is scheduled at a fixed ten-minute activation window, and `accept_start`
refuses any operation whose deadline is further out than the deployment's own
`request_deadline`, so a twenty-second deadline is refused at admission rather than
expiring mid-load. The readiness bound itself is `min(step deadline,
initialize_timeout)`, and `initialize_timeout` has a thirty-second floor and a
nine-hundred-second standalone setting that no document can lower.

The scenario as committed asserts the bound that does exist — the refusal is
definite, nothing was launched, and nothing is owed. Exercising the readiness
timeout against a live engine needs a lever that does not exist yet, and deciding
whether to add one is work for after run 1.

## 2026-09-21 — SGLang launch gate blocked — 5d07f11

Command: `bash scripts/live/run-on-spark.sh host-a sglang`.
The synchronized working tree included pre-existing uncommitted changes; this is
not a clean-checkout result. Launch/configuration fixes are committed in `047007a`
and runner/evidence changes in `5d07f11`.

SGLang 0.5.19, qwen3-4b-instruct, host-a. The runner explicitly set
`MLLM_DEEP_PARK=on`. Wrapper ancestors passed the permission inspection after
synchronization. Release binary checks passed. The gate failed in 4.43 seconds:
Initialize armed and reached the protected wrapper, which reported
`sglang_startup_failed: source_revalidation_failed`. No Ready or inference result
was established, so SGL1–SGL3 did not pass.

Read-only inspection of `~/mllm-sglang-f2-venv/lib/python3.12/site-packages/sglang/srt`
found `unsafe_file`: package directories have mode 0775 and selected source files
have mode 0664. Seven of ten audited files also disagree with the pinned source
hashes: `server_args.py`, `managers/scheduler.py`,
`managers/detokenizer_manager.py`, `managers/scheduler_components/weight_updater.py`,
`entrypoints/engine.py`, `entrypoints/http_server.py`, and `platforms/__init__.py`.
The version string alone therefore does not establish the pinned recipe.

The failed operation is `launch_failed`; the deployment is stopped with admission
and dispatch disabled. A subsequent process check found no engine processes.
No engine installation, driver, or host reboot was changed. The native source
gate remains closed; an approved matching protected installation is required
before a rerun.

Evidence: `target/live/20260921T213446Z/` on control-host contains the test output,
wrapper-path inspection, results, and sanitized engine startup log. The private
controller state remains on host-a at `$HOME/.tmphQyl3M` for diagnosis.
This failed native attempt and earlier CPU/Fake passes are not qualification.
