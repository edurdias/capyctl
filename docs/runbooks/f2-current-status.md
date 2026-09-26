# F2 continuation status

F2 is not complete. Work continues on `feat/f2-sglang`; no push or final merge is
claimed. The current user instruction is one consolidated review at the end,
not per task. Focused TDD and integration verification continue throughout.

## First-run UX, second pass, and management contexts — 2026-09-26 (branch `fix/first-run-ux-2`)

Fixes from a first-run walk with the real binary, each with a regression test
that failed first, plus the owner's 2026-09-26 decision on client contexts.
CPU tests, fake installations and scripted management APIs only; none of this
is qualification of an engine recipe. The host session fix was also checked
by hand on this machine with fake engine environments (a server and five
hosts, one declaring its discrete GPU as a device domain).

- **`status` names a failed launch.** LAST ERROR falls back to the failed
  latest operation's code and first message line instead of `-`.
- **`start` right after `stop`.** `start --wait` waits, within the start's
  window, for the stop to settle and then starts; a plain start refused
  `runtime_retained` while stopping says so and exits 25 (`still_stopping`)
  instead of 2.
- **`start host`** prints a ready line like standalone's (state directory,
  ingress listener, identity file). A fresh host's first control session was
  refused ("host inventory publication refused") and reconnected: its first
  inventory raced the GPU collector on a host declaring a device domain, so the
  device was published unobserved. A host with no executor (no ingress) sent
  the startup snapshot, older than the observation TTL once installations had
  been measured, and was refused on every session. The first inventory of a
  session now waits (bounded, off the session loop) for a device sample, and a
  host with no executor measures its domains again per session.
- **`init host`** leaves the models directory to the shared default
  (`~/models`, downloads in `~/models/sources`, allowed) and writes the
  resource policy standalone derives from the same machine, so the generated
  document validates as written.
- **Plain runtime wording.** Warnings and validation errors no longer cite
  requirement sections or decision records; the wording gate now scans string
  literals in the CLI and configuration crates.
- **`--output` help** is shown at the top level and on `init` and `invite`
  only; the flag still parses anywhere.
- **Management contexts** (owner decision 2026-09-26). Client commands find
  their management API without `--config`: `--context`/`--config` >
  `MLLM_CONTEXT` > the current saved context > the role running on this
  machine (credentials and recorded address under the state root; with both a
  server and a standalone role recorded, the one that answers). `mllm context
  add|use|list|remove|show`; tokens come from `--key-file` or
  `MLLM_CONTEXT_KEY` and are stored owner-only. Needs owner attention: the
  management API stays loopback-only (network guide, every release), so a
  context's address must be loopback; another machine is reached through an
  SSH port forward rather than a TLS management listener.

## Discrete GPU live re-check after the fix wave — 2026-09-26 (branch `feat/discrete-gpu-network`)

Live re-check of the rows the final review left owed, with the branch's
release build (commits `9193a71..` the commit that records this section; the
two fixes below are `da51752` and `513bade`). The 16 GB discrete-GPU laptop
host ran the standalone role with vLLM 0.29.0 and SGLang 0.5.20 registered by
`mllm engine add`, models Qwen3-4B-Instruct (A) and Qwen2.5-1.5B-Instruct (B),
minimal deployment files; other programs held about 30 GiB of its 61 GiB of
RAM throughout. Host A and host B each ran the same aarch64 build as a
standalone role (engines registered by `mllm engine add`, ports by `--set`),
vLLM and SGLang both on Qwen3-4B-Instruct with `residency: deep` and a 40 GiB
memory request so the two cannot be resident together. Evidence is local and
untracked (`target/live/dgpu2/` on the control-plane host). CPU and
Fake-engine tests are not qualification; these rows are.

| Row | Result |
|---|---|
| DG6 upgrade from 0.1.0-rc.4 | Pass. rc.4 from its release tarball made fresh state on the laptop host (its generated `unified` policy) and served a deployment. After the rc.4 role stopped (engine kept running), the branch build started on the same state: the listener notice once, the document moved to `0.0.0.0:8443` with `standalone.yaml.pre-0.1.0` beside it, "resource policy ... replaced (revision 2): domains [unified] are now [gpu0, system]; stopped with verified cleanup first", "re-sized ...". The rc.4 engine was stopped; the deployment came back as revision 2, stopped, and the first request started it cold (55.6 s, HTTP 200). On the machine's LAN address `/v1/models` gave 200 with the key and 401 without or with a wrong one. A second start printed no notice and re-attached the running engine. |
| DG1 vLLM `host_backed`, observed free RAM | Pass. Every A↔B switch planned the victim a stop up front (`park_does_not_fit`, no failed park), because host RAM could not take the copy: about 29 GiB available less the 12.2 GiB reserve left about 17 GiB, and the other model's system charge (4 GiB process placeholder plus 1.5 × its weights) left less than the victim's copy (A 11.2 GiB, B 4.3 GiB). Cold switches 39–49 s. A alone: card 13 281 MiB in use; explicit park 5.3 s, card → 1 009 MiB (A's process 868 MiB), A's host memory 1.9 → 12.5 GiB, available RAM 29.4 → 18.5 GiB after the wake (vLLM keeps the pinned copy); wake 1.39 s (request included), same process. |
| DG3 SGLang parking | Pass after a fix. The saver library check now warns (`mllm_saver_library_permissions`, `group_undetermined`, `warned`, in the engine log; visible only with `--debug-engine-logs`). Parks then reached the saver observation, and the busy scheduler answered in 0.9–1.6 s against the 1.5 s bound: a late answer after the release left the park `uncertain` (accounting kept), one before it refused the park. After the fix (below): B `deep` park 3.7 s, card 7 110 → 410 MiB, wake 10.5 s; B `host_backed` park 4.8 s, card 7 110 → 410 MiB, process host memory 2.0 → 5.1 GiB, wake 15.0 s; A `host_backed` park 6.9 s (card 11 506 → 494 MiB, host 2.0 → 9.9 GiB), wake 9.7 s; A `deep` switches with B: A parked (released: parked) and woke in 12.7–16.9 s, same processes. |
| M12 retry after a failed launch | Pass. A vLLM deployment with a malformed engine argument: three requests in a row each started a new activation and each answered 500 `activation_failed` ("operation ... failed with code launch_failed") in 10.3 s, with a new operation id each time; none was 409. |
| vLLM memory within its reservation | Pass. A ready: the ledger charges `gpu0` 14 220 787 655 B (13.24 GiB, request plus the 1.25 GiB context charge); the engine process holds 13 160 MiB (12.85 GiB), the card 13 281 MiB in all. |
| Unified boot and UUID cross-check, host A and host B | Pass on both. The policy has one `unified` domain, `gpu0` maps to it, no device domain; `gpu0` carries `physical_gpu_uuid` equal to `nvidia-smi`'s UUID for the device at `0000000F:01:00.0` on each host. |
| Unified switching vLLM ↔ SGLang, deep, host A | Pass. Every request answered 42, no refusal. Ready charge 41.25 GiB (40 GiB request plus 1.25 GiB context), parked 2.0 GiB, managed limit 60.84 GiB. Each switch parked the victim (`released: parked`) and reused the same two engine processes. vLLM park about 1.1 s and released 33.2 of the 38.6 GiB its Ready state took (86 %, MemAvailable); vLLM wake 6.6–7.1 s alone, 12.0 s while parking SGLang; SGLang wake 61.5 s (weights reloaded from disk). rc.4 on this host: park about 2 s, 80–82 % of 25.2 GiB, wake 8.8 s. |
| Unified switching, host B | Pass, same shape. vLLM park about 1.1 s, released 32.7 of 38.1 GiB (86 %); vLLM wake 7.6–9.5 s; SGLang wake 59.4 s; same processes throughout; no refusal. |

Fixes found live (each with a CPU regression test that failed before):

- `da51752`: an SGLang saver read that misses its bound is read again. The
  enrolled source uses the protocol's largest bound (2 s) and makes up to three
  reads, each with a fresh request id; only a whole, bound answer is evidence.
  Test `a_saver_read_that_misses_its_bound_is_read_again` (fixture `stall`
  command). Live: the DG3 parks above.
- `513bade`: a victim stopped because its parked footprint did not fit is
  reported `released: stopped (no room to park)`. It was "host RAM full" even
  for a `deep` SGLang victim whose residue did not fit on the card (seen live
  in DG3). ADR 0019 and the install guide updated.

Verification after both fixes: core suite 1112 passed, 0 failed; `mllm-agent`
all targets 0 failed; Python runtime suite 283 passed plus the known
`TMS_SOURCE_ARCHIVE` fixture error; clippy (workspace, warnings denied) and
`cargo fmt --check` clean.

Open, not fixed here:

- The SGLang saver warning is written only to the engine log, which is empty
  unless the role runs with `--debug-engine-logs`; an operator never sees it
  by default.
- On the laptop host the SGLang scheduler still answers saver reads in
  0.7–2.0 s (it spins its main thread); the retry covers it, but a read that
  misses three times leaves the park uncertain.
- `status deployment` after a failed launch shows the instance's LAST ERROR as
  `-` while LAST OPERATION says `initialize failed (launch_failed)`.

Host state afterwards: the laptop GPU idle (113 MiB, no compute process), and
the rc.4 and branch state roots, `~/.config/mllm` and the extracted rc.4
tarball removed. Host A and host B: no mllm role, engine or GPU compute
process, no tmux session; the state roots, `~/.config/mllm`, the build tree
and binaries removed.

## Discrete GPU live rows DG1–DG7 — 2026-09-26 (branch `feat/discrete-gpu-network`)

Live on the 16 GB discrete-GPU laptop host (one 16 GB card, 61 GiB of RAM of
which other programs held about 30 GiB throughout), standalone role of the
branch's release build, vLLM 0.29.0 and SGLang 0.5.20 registered with
`mllm engine add`, models Qwen3-4B-Instruct (A, 8.04 GB of weights) and
Qwen2.5-1.5B-Instruct (B, 3.09 GB), minimal deployment files (name, engine,
model; residency set per row). Harness `scripts/live/matrix/discrete_gpu.sh`;
evidence in `target/live/dgpu/` on that host (untracked). Commits `ba70c21..`
the commit that records this section (fixes `12719ba`, `f9dd21f`, `aaa0382`,
`1d67977`, `d935dc7`, `d50444b`). Core suite 1104 passed, 0 failed; config,
scheduler, agent, domain and protocol 639 / 0; CLI 304 / 0; clippy clean. CPU and Fake-engine
tests are not qualification; these rows are. The multi-GPU picker has no live
evidence in the repository (one GPU here). The unified-host behaviour is not
testable on this host and was not rerun; the GB10 regression row is pending.

| Row | Result |
|---|---|
| DG1 vLLM `host_backed` | Pass with a host-RAM caveat. A cold 45 s (card 13.2 GiB used). Park 5.4 s: card 13 281 → 1 009 MiB, A's process 868 MiB; A's host memory 1.9 → 12.2 GiB (the pinned copy, 11.1 GB = 1.38 × the weights). Wake 1.4 s (request included), same process. B parked: 778 MiB on the card, 4.1 GB pinned; B's wake 0.8 s. In an A↔B switch A parks, then B's start reclaims (stops) it: with the RAM other programs hold, A's copy plus B's charge does not fit above the 20 % system reserve, so each switch is a cold start (72–78 s). No stall. |
| DG2 vLLM `deep` | Pass. A and B switch both ways reusing their processes: A's wake 11.5–14.6 s, B's 7.4 s, A's park 0.6 s; residues 868 / 778 MiB. |
| DG3 SGLang, both tiers | Serves A (66 s cold) and B. Every park is refused before any effect (`park_refused`, "the engine is not quiescent"): the saver binding refuses the SGLang environment's `torch_memory_saver` library (`unsafe_library`), because the environment's files are group-writable and this host's account database (`sss`) cannot prove the group private. The switch then stops the victim (cold switches 26–38 s). Environment, not product: needs `chmod -R g-w` on that environment by the owner (not done; engine environments are not changed). |
| DG4 vLLM A, SGLang B | Pass for switching both ways (A parked then reclaimed, B's park refused as in DG3, so stopped). A's explicit park 5.3 s, wake 1.4 s. |
| DG5 refusal | Pass after a fix: A with a 12 GiB KV cache derives a 21.7 GB request against 15.8 GB managed; `deploy` accepts it provisionally (exit 0, digest measured after), `start --wait` exits 4 `insufficient_device_memory` 0.6 s after the deploy; no engine started. |
| DG6 network | Key: on this machine's tailnet address `/v1/models` is 200 with the key, 401 without or with a wrong one; bound there, loopback is refused; bound to loopback, the tailnet address is refused. A keyless run beyond loopback was not made (owner rule for this host). Not tested from another machine. `config show` lists each setting with its source (`default`, `yaml`, `set`). Upgrade from 0.1.0-rc.4: the notice appears once, the document holds `0.0.0.0:8443`, `standalone.yaml.pre-0.1.0` sits beside it. **Blocker:** the upgraded standalone then refuses to start (`resource policy revision conflict`, exit 1 `internal`): rc.4 stored a unified policy for this machine and this build derives `system` + `gpu0`; see "Owner attention". |
| DG7 remote host | Not run (optional for 0.1.0); pending. |
| Extra | `restart_only`: every release is a stop, every return a cold start. GPU pin: the engine is started with `CUDA_VISIBLE_DEVICES` set to the card's UUID. vLLM serves A with a fitted context of 26 752 tokens after the context fix. |

Measured, to replace the placeholders: parked device residue 868 MiB (4B) and
778 MiB (1.5B) for vLLM against the 1 GiB placeholder; engine host memory
(anonymous plus shared) 1.8–1.9 GiB for vLLM and 2.0 GiB for SGLang when ready,
against the 4 GiB placeholder; vLLM's pinned `host_backed` copy 1.37 × the
weights, kept after the wake; vLLM's own process holds 13.2 GiB of the card
against a 12.0 GiB reservation (weights, 3.7 GiB of KV cache and about 1.5 GiB of
CUDA graphs and context).

Product fixes found live (each with a CPU regression test; "live" means the
row above exercised the fix):

- vLLM refused a fitted context by one block (its null block): the fit leaves
  one block to vLLM (live).
- A park was charged its whole parking footprint against free memory, so a
  `host_backed` park on a small card was held until its deadline; a park or
  wake is now charged only what it adds beyond the owner's own charge on
  `device` and `distinct` domains, and a domain it adds nothing to is not
  judged on free memory (live).
- A settled parked owner is credited on `device` and `distinct` domains (its
  residue and its copy are in use); `unified` unchanged (live). Reclaiming such
  an owner in a forecast removes its floors with it (CPU).
- A switch park host memory cannot take is refused `parked_capacity` so the
  switch stops the victim, instead of holding the request for 10 minutes (live).
- The `host_backed` copy is charged at 1.5 × the weights in every phase for both
  engines, and the process sample counts shared resident memory, where the
  pinned copy lives (live measurements; ADR 0019 and the operator guide
  amended).
- Admission sampled GPU processes from a cache that was always past its age
  bound, so it credited nothing; each observation takes a fresh sample (live).
- A failed on-demand activation left every later request answered 409
  `idempotency key identifies a different command`; the key names the latest
  operation (CPU; found live).
- A derived request larger than the card answered `checkpoint_mismatch` (exit 2);
  it is `insufficient_device_memory` (exit 4) (live).

Open, not fixed here:

- An SGLang profile takes no host-fixed arguments, so the Triton attention
  backend goes in each SGLang deployment's `extra_args` (`engine add --arg`
  is refused for SGLang).

### Final review fix wave — 2026-09-26 (commits `6766cbd..8781ef0`)

The whole-branch review (`2fbcc48`) found 1 critical and 14 important
issues. Fixed on CPU, each with a regression test that failed before; CPU and
Fake-engine tests are not qualification, so the live rows below are still
owed:

- **Upgrade of a generated policy (C1, the DG6 blocker).** Standalone replaces
  its generated resource policy on the first start that observes another
  machine shape: engines charged under the old policy are stopped by the
  ordinary Stop first (only verified cleanup releases them), the policy, keys
  and epoch change in one transaction, deployments are re-sized from their
  stored documents (one that names the old domain is listed with what to do),
  and a one-time notice says so. A hand-written host policy is never replaced;
  a changed shape is refused with the recorded and declared domains and the
  recovery steps. ADR 0019 §10a. Live: pass (re-check section above).
- **Planner and memory (I3, I4, M9).** The device domain is charged the
  request plus the engine's CUDA context and graphs (1.25 GiB placeholder;
  vLLM held 13.2 GiB against 12.0), so vLLM now needs a card of about 10 GiB
  or more; a smaller card boots and refuses each vLLM deployment. Park or
  stop is decided from the host's fresh observation as well as the ledger, so
  DG1's host_backed parks that host RAM cannot take are planned stops; a park
  refused for memory is reported `stopped (host RAM full)` (now
  `stopped (no room to park)`). Live: DG1 pass (re-check section above).
- **One rule for every host shape (I5, owner rule 2026-09-26).** Resident
  crediting (parked owners, a park's own charge, `RssShmem`) and the
  switch-park refusal are the same on unified and discrete hosts. Live: the
  unified switching row passed on each lab host (re-check section above).
- **SGLang saver permissions (I2).** A library that fails the owner-only rule
  is observed with one warning in the engine log instead of refused. Live:
  DG3 parks after a further fix (re-check section above).
- **Configuration and network (I1, I8, I9, I10, I11, I12, I13).** A
  read-only document keeps the new inference default while the migration is
  pending; the server takes `--management-listen` / `MLLM_MANAGEMENT_ADDR`,
  clients find a role by its recorded management address, a disagreeing
  state root is refused, `join host --set` works; `devices: [{id: gpu1}]`
  parses; `validate config` runs the start's standalone checks and reports
  unknown weights as unknown; ready lines name the credentials file; the
  tarball ships the linked guides with a link check; help and shipped
  documents carry no process wording (a gate enforces it).
- **Heterogeneous GPUs (I7).** A GPU too small for a model is excluded for
  that deployment; the host is refused only when no GPU fits. No multi-GPU
  live row exists.
- **Minor (M4, M10, M11, M12, M19) and tests (I14).** `device_unobserved` has
  a hint; status names the deployment's installation; a request while the
  checkpoint is measured is a retryable "starting"; concurrent arrivals join
  one activation; the flaky native vLLM tests take ports outside the ephemeral
  range; every CLI test runs against an isolated home.
- **Not done here (I6).** The GB10 PCI/UUID cross-check has a fixture with
  both collectors' formats; live: the unified standalone boot on each lab
  host published the matching UUID (re-check section above).

## Single-box benchmark through mllm — 2026-09-25 (branch `test/model-benchmark`)

Owner-approved experiment: five models, 256 in / 256 out, one user, vLLM 0.29.0
on host A and SGLang 0.5.20 on host B, deployed by mllm and requested through
the router (row M80, bench phase, 1 warmup + 5 measured). Full method, flags,
results and failures: `docs/benchmarks/2026-09-25-single-box.md`. Evidence:
`target/live/bench/` on the control-plane host, run `matrix-20260925T125244Z`.
Not qualification.

- **Live results (decode tok/s, median; baseline → best drafter).** MiniCPM5-2B
  36 → 85 (DSpark, both engines); Qwen3.6-35B-A3B NVFP4 77 → 126 (vLLM DFlash),
  85 → 125 (SGLang MTP); Ling-3.0-flash int4 23 → 48 (SGLang DSpark; vLLM not
  run, needs `trust_remote_code`); Gemma-4-E2B 38 → 96–100 (assistant);
  Qwen3.8-27B NVFP4 10.5 → 27.6 (DFlash2). mllm path overhead 25–85 ms at first
  token, 25–55 ms at stream end.
- **Hugging Face sources worked live** (first use): 17 sources, about 123 GB
  per host, resumed across three host-role restarts; the Wi-Fi link (about
  9 MB/s per host) set the pace.
- **Product fixes, each with CPU regression tests:**
  - Exercised live: vLLM `--speculative-config` is admitted key by key under
    host approval, at deploy time and at launch; it was classed as a path, so
    no vLLM speculation could deploy. Accepted as ADR 0014 Amendment A3, with
    an "Amended by" note in SPEC §8.2. A source copy already verified on the
    host is reused by a new deployment; activation had been refused
    `model_source_pending`.
  - After the owner's decisions, not run live: vLLM needs `nvcc` on PATH to use
    FlashInfer. The CUDA PATH that fixed this live is now an optional,
    host-approved profile field, `cuda_home`: `engine add` detects it, and
    standalone takes `MLLM_CUDA_HOME` (SPEC §13.3 amendment).
  - After the owner's decisions, not run live: mllm sets
    `MAX_JOBS = clamp(floor(MemAvailable / 8 GiB), 1, CPUs)` and
    `FLASHINFER_NVCC_THREADS=1` at launch, logs the choice, and a profile's
    `env` may override either one.
  - After the owner's decisions, not run live: `deploy --activate` and
    `start --wait` wait for a model source that is still downloading, within
    the Initialize window.
- **Still open:** Ling on vLLM needs checkpoint code (`trust_remote_code`),
  which stayed off. SGLang engine output is only captured with
  `--debug-engine-logs`. The CUTLASS fused-MoE JIT ran both hosts out of memory
  once during the run; the new `MAX_JOBS` bound has not been exercised live.

## First-run friction from the guide walk — 2026-09-25 (branch `fix/first-run-friction`)

Four fixes before 0.1.0, found by walking the user guides with the real
binary. Regression tests drive the `mllm` binary (`tests/first_run.rs`,
`tests/validate_config.rs`); CPU, fake installations and a scripted role and
management API only, so none of this is qualification. No live run was made.

- **`engine add` with no role running exits 0** (ADR 0018 §3). No control
  socket, or a stale one refusing connections, means no role runs (the normal
  first run: standalone refuses to start with no engine). The profile is saved
  and the command prints `saved to <engines.yaml> (revision N); start mllm
  (…) to use it` with `published: role_not_running`. A role that is running
  but does not take or answer the request still exits 22.
- **`deploy model --activate` waits for the checkpoint digest** (ADR 0014
  §7). `--activate` (and `start deployment|instance --wait`) poll status while
  the revision's digest is pending, bounded by the start's Initialize window,
  then start. Nothing is started if the bound passes. A plain deploy stays
  asynchronous and names `mllm start deployment <name> --wait`; a start
  refused `checkpoint_digest_pending` says the same.
- **`validate config --host` runs deploy's per-host checks** (ADR 0013 §2–3,
  ADR 0018 §7). It merges the `engines.yaml` beside the host document, and
  refuses a placement selector the host's labels do not match, a host with no
  runtime profile, and a profile the host does not declare
  (`profile_not_published`, exit 24). It also resolves the scoped documents as
  the server does. The result's `requires_server` lists what only a running
  server checks.
- **Relative `--config` and `$MLLM_CONFIG` are made absolute** (ADR 0018 §2)
  once, before any command or role uses them. A bare `host.yaml` had put
  `engines.yaml` at an empty parent directory, whose sync failed after the
  write, so the profile was saved but never published.

## Context fitted to the KV grant; standalone rendezvous root — 2026-09-25 (branch `fix/context-fit-standalone-rdzv`)

Two owner decisions of 2026-09-25. CPU tests only; live proof on real engines
is pending (no live run was made: the hosts were busy with a benchmark).

- **Context fitted to the KV grant** (ADR 0014 §5). With no
  `engine_config.context_length`, the shared launch builders compute the
  largest context the KV cache grant holds from the checkpoint's `config.json`
  (layers, KV heads, head dim; KV element width from `kv_cache_dtype`, then
  `dtype`, then the checkpoint's, fp8 at one byte), cap it at
  `max_position_embeddings`, round it down to a 16-token block (or
  `vllm.block_size_tokens`) and pass it as vLLM `--max-model-len` or SGLang
  `--context-length`, for every profile on both engines. Sliding-window and
  hybrid layers count as full attention; MLA, missing fields, an unknown KV
  dtype or a missing `config.json` fall back to 4096 with the reason. An
  explicit value wins (with a warning when the grant provably cannot hold
  it); a host-fixed `--max-model-len` in `MLLM_ENGINE_ARGS` is kept. The
  standalone `--max-model-len 4096` environment default is removed. The fit
  runs where the checkpoint is (embedded host or host agent) and is not part
  of the effective configuration. `validate config` shows `effective.context`;
  `status` shows each deployment's `context` (`declared`, `host_fixed`,
  `fitted`, `fallback`, or `on_host` for a remote host's revision).
- **Standalone rendezvous root** (SPEC §8.2 / T21). Standalone creates
  `<state>/rendezvous` (0700, refused if not owned/0700, as a host) at start,
  names each SGLang launch's rendezvous directory in it, removes it once the
  launch's processes are proved gone, and at start sweeps directories no
  retained binding owns (never through a symlink, never outside the root).
  The directory name matches the host's (`rendezvous`), not `rdzv`.

Live checks still owed: a vLLM and an SGLang standalone deployment with no
`context_length` start and report the fitted value; an SGLang stop leaves no
directory in `<state>/rendezvous` and none in `/tmp`.

## Table output for record views — 2026-09-25 (branch `feat/cli-table-output`)

Owner decision 2026-09-25 (recorded in SPEC §14): like the docker CLI, commands
that read records print an aligned table by default, terminal or not: `list
hosts`, `list deployments`, `list engines`, `status deployment` (the
deployment, then its instances), `engine list` and `engine detect`. Upper-case
headers, host names resolved from the host inventory (the id when a host has
none, or the inventory cannot be read), memory in GiB, timeouts in seconds;
nested detail stays in the JSON. An empty result prints the headers only.
`--format json` (or `--json`) prints the JSON result byte for byte as before
and reports errors as JSON, exactly as `--output json` did; `--output json`
is still accepted. Mutations, `inspect`, `validate`, `prune`, `drain` and
`revoke` print JSON as before; exit codes are unchanged.

- Rendering: `crates/mllm-cli/src/table.rs` (unit tests: alignment, empty
  results, host-name resolution and fallback, units, host states, status
  sections, engine views). Binary tests: `management_cli` (T10: `list
  deployments` and `status deployment` tables; `--format json`, `--json` and
  `--output json` print identical bytes), `engine_cli` (T37: `engine detect`
  and `engine list`), `host_recovery` (`list hosts` names a revoked host).
- Every CLI test that parses JSON passes `--format json`.
- Live matrix: every script passes `--format json`. `lib.sh`'s `cli` probes
  the binary once and translates the flag to `--output json` for a release
  from before this change (ENG4's rc.3 binaries, release validation).
- Verified locally: workspace and core suites, clippy with warnings denied,
  `cargo fmt --check`, `scripts/test-install.sh`, `scripts/verify-packaging.sh`,
  harness dry-runs of M73, M08 and ENG1 to ENG4 (M54's dry-run fails the same
  way on `main`). CPU and Fake-engine tests only; no live run, and nothing
  here qualifies an engine recipe.

## Engine registration — 2026-09-25 (branch `feat/engine-registration`)

ADR 0018 (amends SPEC §4.2, §15.1): `mllm engine detect|add|list|remove` and
`mllm list engines`, live publication (`live_profile_update`), two-phase
removal, standalone `local-vllm`/`local-sglang`. Commits: `deed9ae..HEAD`.

Evidence: CPU and Fake-engine tests only (`crates/*/tests/{registration,engines,
control_socket,live_profiles,engine_cli,standalone_engines}.rs`, plus a
hard-link hardening regression pair added in `registration.rs` this pass).
These are not qualification. Live rows ENG1–ENG4 ran on 2026-09-25; see
"Live rows ENG1–ENG4" below.

Owner decisions of 2026-09-25 are recorded in the plan and ADR 0018; the answer
on the exit code for `profile_not_published` (plan item 20): fail-fast, HTTP
409, CLI exit 24 — the next free exit number, following the accepted 16–23
pattern for the other engine-registration codes.

Final review (local review notes,
range `612ed0d..dc1a426`): 1 Critical and 5 Important findings, plus 12
minors; verdict was not ready until fixed, plus live ENG1–ENG4. A fix wave
(`dc1a426..e7565c1`) addressed the Critical — the role no longer writes
`engines.yaml`; the CLI writes it only after the server confirms the
retirement, running as root under `sudo` for the packaged system units — and
all five Important findings: retirement idempotency so a retry resumes
instead of conflicting, standalone expiry and placement exclusion via store
v36, and an unanswered remove reported as an unknown outcome instead of
"nothing was removed", plus formatting drift and flaky environment-variable
tests. A re-review
(local review notes) read the
fix diff against every finding and ruling and confirmed C1 and I1–I4 fixed
with no new Critical or Important breakage; I5 (the live rows) stays open by
ruling. This session's pass fixed the re-review's Minor 1: a root CLI opening
`engines.yaml.lock` followed a hard link and could `fchown` the file it
pointed at; the lock and the engines file are now opened `O_NOFOLLOW` and
refused unless they are a regular file with one link owned by root or the
state-dir owner, checked before any `fchown`, with a regression test that
fails against the prior code.

Decisions from the plan's local decision record:

- No pre-flight cross-task conflict scan; the owner removed per-task review for
  speed, and the plan's self-review checked type consistency.
- Implementers run the task's own tests plus a build of touched crates; the
  core suite, workspace tests, clippy and fmt run before the final review.
- `profile_not_published` is HTTP 409, CLI exit 24 (fail-fast; see above).
- `expire_profile_retirements` expires only rows in state `retiring`; a
  confirmed retirement stays until the host's re-publication drops the
  profile, so a stale confirmed row cannot block re-adding the same name
  forever.
- An uncertain or unfinished stop keeps a retirement open until it expires
  unconfirmed — the spec never confirms on a guess.
- Any accepted host publication, startup or live, clears that host's
  confirmed retirement rows for profiles the publication no longer lists, so a
  host restart cannot strand a confirmed row.
- The drain poll in the session relay has no bound of its own; the retirement
  service returns Holding (unconfirmed) once the 900 s bound passes, so a
  drained removal cannot poll forever.
- The role binding the control socket must first check its parent state
  directory is owned by the running user, mode 0700, and refuse to bind
  otherwise, closing the bind-to-chmod umask window.
- `engine add` works before any role has ever started (the first-run path):
  the state root is created owner-only (0700) if missing, `engines.yaml` is
  written, and the command reports `agent_unreachable`.
- Roles resolve `engines.yaml` with the same rule as the CLI, including
  `$MLLM_CONFIG` when `--config` is absent, so a role and `mllm engine` always
  agree on the file.
- (Outside this plan) A standalone vLLM deployment generated by the template
  never parked, because residency ignored the deep-park switch regardless of
  it — contradicting the owner rule that standalone must not differ from
  server mode; fixed on a separate branch and live-verified with rc.4.
- (C1) The CLI is the only writer of `engines.yaml`; the running role never
  writes it. Removal: the CLI asks the role to retire, waits for the
  confirmation, then writes the file and asks the role to reload. Under the
  system units the operator runs `sudo mllm engine … --config
  /etc/mllm/host.yaml`; the CLI keeps an existing file's owner and mode, and
  creates a new one for the role's service user, mode 0600, so
  `ProtectSystem=strict` never blocks the role. A crash between the
  confirmation and the CLI's write leaves the profile in the file while the
  server keeps it out of placement; running `remove` again finishes it.
- (I1) A profile retirement's idempotency key is stable per (host, profile
  name) until the retirement is cleared, so a retried `remove` resumes the
  same retirement instead of conflicting with it; an accepted publication
  that no longer lists a profile clears its confirmed row, on both the
  startup and the live path.
- (I2, I3) Standalone runs the same expiry and the same placement exclusion
  as the server, including a publication row (store v36), so a removed
  profile can never start again and a crash mid-drain does not wedge the
  name.
- (I4) When the CLI loses the connection or times out during `remove`, it
  reports the outcome as unknown, not "nothing was removed", and tells the
  operator to run `mllm engine list`; the client's wait bound carries a
  margin over the role's 960 s plus the drain timeout.
- (I5) Live rows ENG1–ENG4 ran on 2026-09-25 (below).

Live rows ENG1–ENG4, 2026-09-25. Host A is the first host, host B the
second; the server runs on the control-plane host. The branch was built from
the synced tree on all three machines, and only the existing vLLM 0.29.0 and
SGLang 0.5.20 environments were used. Evidence (local, not committed):
`target/live/engreg/`.

| Row | Live verdict |
|---|---|
| ENG1, host A and host B | pass on both. `engine detect` with no `--path` listed both home-level environments from metadata. The host ran under a transient systemd user unit with a document that declares no profiles. `engine add` of each environment exited 0, `published`, `custom: false`: vLLM in 12.2 s and 12.8 s, SGLang in 7.3 s. These times are the whole command, including the bounded version check and the deep-park probe; the plan's "within 10 s" is not met for vLLM, whose version check alone takes several seconds. `host.yaml` was byte-identical afterwards (`sha256sum -c` OK), and `engines.yaml` reached revision 2. `list engines` on the server showed both profiles. v\*-4 and s\*-4 on the new profiles reached Ready, answered 42, stopped with verified cleanup and were deleted |
| ENG3, host A | pass. With va-4 Ready, `engine remove vllm` was refused `profile_in_use`, naming va-4; the deployment stayed Ready and answered. `engine remove vllm --drain` returned in 1.2 s (`removed: vllm`, revision 2); the deployment was stopped with verified cleanup. `list engines` no longer showed vllm. `start --wait` was refused `host_ineligible` ("no approved configuration carries a runtime profile whose build it reported"). `engine add` published the profile again (revision 3) |
| ENG2, host A (standalone, both variables set) | pass after the fix below. `engine list` shows `local-vllm` and `local-sglang`, `published`, `source: environment`. `engine remove local-vllm` is refused `invalid_config` (environment profile). A deployment on `local-vllm` served: its argv carries the host-fixed `--max-model-len 4096`. A deployment that also states `context_length` is refused `invalid_config` ("the installation's host-fixed args already set `--max-model-len`"). A deployment on `local-sglang` served. `engine add --name vllm-reg` of the same vLLM environment was published beside them. See also the registered-profile check below |
| ENG4, host B | pass. An rc.3 agent was online (`supported`) with the new server. The new CLI's `engine add` exited `agent_unreachable` and wrote `engines.yaml` (revision 1). After a restart the rc.3 agent still published nothing from that file. After an upgrade to the new binary, the host published `vllm` at start. Against an rc.3 server, a new agent's `engine add` answered `published: restart_required`, and after a host restart the rc.3 server listed `vllm` for the host |

Registered vLLM profile without a `--max-model-len` default (ENG2). A profile
added with `engine add` carries no arguments, so the environment profile's
`--max-model-len 4096` default does not apply. A deployment on it that states
`context_length: 16384` passes `--max-model-len 16384` and serves. A deployment
that states no context length launches with the model's own maximum.
qwen3-4b-instruct has a 262144-token context, so vLLM refused at start:
"To serve at least one request with the model's max seq len (262144), 36.0 GiB
KV cache is needed, which is larger than the available KV cache memory
(4.0 GiB)". The operation failed as `launch failed: the engine exited before
readiness`, with the engine_config hint, and cleanup was clean. This is
vLLM's own limit, not an mllm defect. An operator who registers a vLLM
environment must state `context_length` in each deployment, or add the
profile with `--arg --max-model-len --arg N`. Whether the documentation
should say so, or a registered vLLM profile should carry the same default
as the environment profile, is an owner decision.

Found and fixed during the live rows:

1. **Product: `engine list` did not show standalone environment profiles.**
   The first ENG2 run returned an empty list. `list` read only `engines.yaml`
   and the role document, and dropped the profiles the role reported as
   accepted from `MLLM_VLLM_BIN`/`MLLM_SGLANG_BIN`. It now appends every
   profile the role accepted that neither file names, with
   `source: environment`. Regression test (T16):
   `engine_cli.rs::list_shows_the_roles_environment_profiles`, which fails
   without the fix. The rerun of ENG2 shows both profiles (live proof).
2. Harness: `roles.sh` `server_init` copied the snapshot build over
   `MLLM_LOCAL_BIN`, so ENG4's "rc.3 server" binary was replaced by the new
   one. A named binary is now run as given. ENG4 also read `hosts.json` for
   `"vllm"` anywhere, which the other host's full document matched; it now
   checks the named host's entry only. Its rc.3-server phase now reloads the
   new run's variables and runs in a subshell.
3. Harness: `host_clean` hid leftover rendezvous directories. `ls -d A B &&
   echo LEFTOVER_RDZV` exits non-zero when one pattern matches nothing, so
   the directories were listed but never flagged. It now pipes through
   `grep .`.

Found, not fixed (product, open): **a standalone SGLang launch leaves
`/tmp/mllm-rdzv-*` behind.** Both ENG2 runs left one owner-only
`/tmp/mllm-rdzv-*` directory, holding its `store` file, per `local-sglang`
launch after `delete --stop`. The directories were created at the SGLang
launch times, and they were removed by hand after the run. The host role
names each launch's rendezvous directory inside
`<state_dir>/rendezvous/<incarnation>` and removes it on gone evidence (fixed
2026-09-23). Standalone never sets a rendezvous root (`ProfileBindings` has
none), so the entry falls back to its own temporary directory, and a
signalled stop never runs the entry's exit handler. This predates engine
registration (same path on `main`) and breaks the rule that standalone must
not differ from server mode. A fix needs standalone to own a rendezvous root
and to retire each launch's directory where its local cleanup proves the
group gone (standalone does not retire saver enrollments there either). That
is a design choice in the coordinator's local cleanup, so it is left for the
owner. With the `host_clean` fix, ENG2's idle check now fails on this until
it is fixed.

Local verification of the fix (CPU only, not qualification): `mllm-cli`
all-targets 208 passed; Clippy on `mllm-cli` clean with warnings denied;
`cargo fmt --check` clean.

Host state after the rows: every role was stopped (`roles.sh down`, each
role signalled by its recorded identity). The branch's trees, run directories
and the rc.3 binary copy were removed from both hosts. Neither host has an
mllm, engine or GPU compute process, a tmux session or a rendezvous directory.
`~/.config/mllm` is absent.

Open items:

- Standalone SGLang rendezvous directories (above).
- Registered vLLM profile and context length (above): owner decision.
- Version skew on removal: an older CLI's `remove` can still reach a newer
  role and get back a confirmed retirement, but does not know to write
  `engines.yaml` on this path. The retirement keeps the profile out of
  placement, but it lingers in the file until a current CLI runs `remove`
  again. Worth a release-notes callout.
- Minor findings left unfixed this round: `final-review.md` minors 2–12, and
  `final-rereview.md` minors 2–6 (a `standing`-map entry that outlives a
  resumed-then-cleared poll; standalone `reload` committing the embedded
  publication and `host.replace` separately; `publish_at_start` collapsing
  every store error into one message; a rename race in the control socket's
  directory-owner read; a loose `"1 passed"` substring match in an isolated
  test). `final-rereview.md` Minor 1, the `engines.yaml.lock` hard link, is
  fixed as of this session's commit.

## Release candidate 0.1.0-rc.4 — build and live pass, 2026-09-25 (branch `docs/rc4-live-evidence`)

Host names in this section: host A is the first host, host B the second (the
tight-policy host); the control-plane host runs the server. Evidence (local,
not committed): `target/live/rc4/`.

**Cleanup of the observation debug run.** The roles left from run
`matrix-20260925T021319Z` (both host agents in tmux and the server) were
stopped with `roles.sh down` (each role signalled by its recorded identity and
exited). The debug tree `~/mllm-obsfail` and the run directory were removed
from both hosts. Afterwards neither host had a tmux session, an mllm, engine or
GPU compute process, or a rendezvous directory.

**Release.** Draft pre-release `v0.1.0-rc.4` (unpublished; the owner
publishes) now targets `main` at 612ed0d (PR #28 drain fix and PR #29
standalone vLLM deep park). Both tarballs were rebuilt from 612ed0d: x86_64 on
the control-plane host, aarch64 natively on host A (nice 19). Each passed
`scripts/verify-packaging.sh` on its own architecture (shellcheck is not
installed there, so that check was skipped). `BUILDINFO` records commit
612ed0d, not dirty, and runtime manifest `dec9dca0…91561`. The assets were
replaced and the notes updated to add #29.

| Asset | sha256 |
|---|---|
| `mllm-0.1.0-rc.4-linux-x86_64.tar.gz` | `69d3e9ce61a45c72243134257d7319e0ded8634483ce0c7a26b5993257b69a24` |
| `mllm-0.1.0-rc.4-linux-aarch64.tar.gz` | `df08d2b2da12c99126936ad9d8c917d4d5d869aea2093023b9f69a65a37d8b63` |
| `install.sh` | `a83b56109bba710a84fa3dfb5919c1caebc5c8c980c12cd81c8a3a0fae2812d0` |
| `SHA256SUMS` | `bcb272df7eac00b1ad04c1f220d08d8e206196bd1a8f5c1dcde8d00fdb33b823` |

**Live pass, installed binaries only.** Every role ran from binaries installed
by `install.sh`, fed from a `file://` mirror of the draft's assets (downloaded
back from the draft and checked against its `SHA256SUMS`). The roles ran under
the packaged systemd user units with fresh state: the server's state in its
own directory, the hosts in the default layout (`~/.local/state/mllm/host`,
document at `~/.config/mllm/host.yaml`, managed runtime). Only the matrix
harness scripts were copied to the hosts, with no build or runtime tree. Host A
used the normal budget, host B the tight one (managed limit 84 GiB,
`max_parked` 1). vLLM 0.29.0 and SGLang 0.5.20 ran from the existing
environments.

| Check | Live verdict |
|---|---|
| a. M73 va-4, sa-4 | pass. Both engines launched from the managed runtime, listening on loopback only (refused from off-host). Unkeyed engine and control routes returned 401; the router refused unkeyed callers (401) and has no `/metrics` path (404). The answer was correct and the stream well-formed. Stop cleaned up with verification. The restart made a new binding with no reused PID. MemAvailable came back within 0.31 GiB |
| b. Standalone vLLM, generated deployment (#29) | pass on host A, `mllm start standalone` under the packaged standalone unit. The deployment document came from the product's own template (`standalone_config::deployment_document`, deep parking on), which gave `residency: deep` and `--enable-sleep-mode`. Two park/wake cycles: each park settled `parked` in about 2 s and released 20.2–20.5 GiB of the 25.2 GiB Ready drop (80–82%, MemAvailable). The same two processes kept their PIDs and start ticks. A routed request woke the deployment in 8.8 s with the right answer, still at generation 1. A `systemctl --user stop`/`start` of the unit re-attached the running engine. `delete deployment --stop` cleaned up |
| c. `drain host` with `request_deadline: 600s` (#28) | pass on host B. Status showed `request_deadline_ms` 600000. The drain returned `drained: true` with the instance `stopped` and cleanup `verified`, in 1.1 s with nothing refused. Cleanup was verified with no engine, GPU process or port left. The next request reactivated the deployment on demand (20 s, answer correct). Before #28 this drain was refused `LifecycleConflict` |
| d. Refusals (#18) | pass. After an operator stop, a routed request got 409 `deployment_stopped`, and the message names `mllm start deployment <id>`. Host B then ran rc.1 under its unit on the rc.4-written state, and the server listed it `upgrade_required` ("reports no version"). `start deployment`, `start --evict` and `start --wait` were each refused `host_ineligible`, CLI exit 15. The message names the host, "host version unreported, server version 0.1.0-rc.4" and the reason. The deployment stayed stopped. Reinstalling rc.4 made host B `supported`, and the start served |
| e. Two-instance `start --evict --wait` on tight host B (#18) | pass. Incumbent vb-14 (46 GiB) was Ready and vb-4 with 2 instances (2 × 24 GiB) was deployed. `start --evict --wait` exited 0 in 22.6 s with both instances Ready. Its receipt names the one victim (`vb-14/0`), which was parked, not stopped: the tight host allows one parked deployment. Both requests were answered. An earlier run first tried a plain `start --wait`: instance 0 fit next to the incumbent, instance 1 did not, and after the 900 s start deadline it exited 4 (`insufficient_resources` … "the start is partial") with instance 0 left Ready. That matches SPEC §14 |
| f. `revoke host` under the packaged unit (#17) | pass on host A with va-4 Ready. The revoke answered `engines: retained`. The agent logged one `error [host_revoked]` line naming both recovery commands. systemd recorded `status=14`, `Result=exit-code`, `NRestarts=0`, and the unit was still failed 20 s later (not restarted). All three engine identities stayed alive, and dispatch returned 503. Then `invite host <id> --recover` and `join host --recover` (same host id, `recovered: true`) and a unit start: online, Ready, the same three PIDs and start ticks (re-proven, not relaunched), and served |
| g. M28 sa-14 SGLang park/wake ×3 | pass 3 of 3. Each run released 88.7–88.9% of the Ready drop with the same four processes, woke on request in 182–219 s (disk reload), and matched I1 exactly (max logprob delta 0.0). The host journal for the run has 0 `native_observation_*` or `saver_observation_*` lines, including the new `native_observation_slow` |

Scratch rows used for c–f (`DRAIN600`, `EVICT2`, `EVICT2D`, `SKEWD`, `SKEWX`,
`REVOKE`) are kept with the evidence under `target/live/rc4/rows/`, and run
through `SCRATCH_ROWS`. The first `SKEWD` run shows rc=1 because its exit-code
wrapper checked the wrong status; `SKEWX` repeated the refusals and recorded
exit 15 from the CLI itself.

No product bug was found. CPU and Fake-engine tests are not qualification. The
rows above prove the named behaviours only for the q4 and q14 fixtures on
these two hosts.

Host state after the pass: every deployment was deleted and every unit stopped
and uninstalled. `~/.local/state/mllm`, `~/.config/mllm` and the copied
harness and mirrors were removed from both hosts. Neither host has an mllm,
engine or GPU compute process, a tmux session or a rendezvous directory. The
server state stays on the control-plane host under `~/mllm-rc4-server`.

## Standalone vLLM deep parking — 2026-09-25 (branch `fix/standalone-vllm-deep-park`)

Live-proven with rc.4 (see the rc.4 section above, check b). The original
evidence was CPU and Fake-engine tests only.

- Defect: the generated standalone deployment declared vLLM `restart_only`
  whatever the deep-park switch said, so a standalone vLLM deployment launched
  without sleep mode and never parked; idle eviction and switching stopped it
  cold. Server mode deep-parks the same engine (live-proven earlier: 77% of its
  memory released with the same processes). Owner rule: standalone must not
  differ from server mode.
- Fix: the template's residency follows the host's switch for every engine
  (ADR 0012, SPEC §6.2): `deep` when deep parking is on (`MLLM_DEEP_PARK` unset
  or `on`), `restart_only` when the host opts out. A build whose probe finds deep
  parking missing is refused `capability_missing:deep_park` by the protected
  entry, as elsewhere; the host then opts out and gets `restart_only`.
- Tests: a standalone boot with deep parking on parks its generated vLLM
  deployment and wakes the same launch (same endpoint, per-launch key and live
  processes); an opted-out boot's deployment is `restart_only` and its park is
  refused. The Fake gained an opt-in mode that reports real processes as its
  group and follows the embedded vLLM residency contract; the default Fake is
  unchanged. Workspace 1788 passed, 1 ignored; core suite 1012 passed; Clippy
  clean.

## Flaky parallel tests and leaked stand-in engines — 2026-09-25 (branch `fix/flaky-parallel-tests`)

CPU-only test fix; nothing here is live-proven or qualifies an engine recipe.

- Root cause of the intermittent failures (launcher group observation, agent
  `journal` and `native_vllm` readiness): `observe_process_group` failed with
  `Visibility` whenever any unrelated process on the host exited between the
  `/proc` listing and its `stat` read. Under a parallel test run that was nearly
  every scan (a churn regression test failed 200 of 200 observations). It now
  skips a pid that no longer exists, as `scan_group_by_pgid` already did; any
  other unreadable process still fails closed, and a member leaving between the
  two snapshots is still `Changed`. The same failure applied to production
  observations on a busy host.
- Leak: `ready_deep_park` asserted readiness before its callers installed the
  reap guard, so a failed readiness left the stand-in vLLM running (orphans found
  on control-host). The guard is now part of the test `Host`, and the stand-in ends its
  own group if the test process or its fixture directory disappears.
- Evidence: workspace at default threads failed 2 of 10 runs before the fix;
  launchers + agent at 32 threads failed 3 of 10 before (each failure leaked an
  engine) and 0 of 20 after, with no new leaks.

Owner decisions 2026-09-25, both implemented; CPU and Fake-engine tests only, not
live-proven on the hosts.

- A start that places nothing because no allowed host is eligible (drain-only
  after version skew, draining, revoked, offline, reconciling) is refused
  `host_ineligible` (HTTP 503, CLI exit 15), naming each host and why, with the
  host's and the server's versions for a drain-only host. It was
  `capacity_blocked`. `start --evict` checks this before releasing anyone.
- An inference request for an operator-stopped deployment is 409
  `deployment_stopped` (message names `mllm start deployment <id>`). It was 429
  `insufficient_resources`. New code, added to SPEC §10 in the same change;
  `host_ineligible`, the `--evict` coverage and the `--wait` rule are recorded
  in SPEC §14.
- `start deployment --evict` plans every instance the start activates before
  releasing anyone (each against the ledger the earlier ones leave, each victim
  set minimal) and releases per host. If one instance cannot be placed even with
  eviction, nothing is released and the refusal names the instance and the
  host's need, free and evictable memory (`capacity_blocked`, exit 4).
- `start deployment --wait` succeeds only once every instance has been Ready; an
  instance not placed before the start's deadline exits 4, a failed launch 13.

Regression tests (each failed on `main` before the fix): management `evict.rs`
(every replica evicted for, refusal before eviction, `host_ineligible`), router
`router_core.rs` and CLI `stop_intent.rs` (409 `deployment_stopped`), CLI
`start_wait_replicas.rs` (partial start exits 4). Pending: a live run of a
two-instance `start --evict --wait` on a tight host and of a start against a
drain-only host.
## A revoked host exits instead of retrying — 2026-09-24 (branch `fix/revoked-host-exit`)

Owner decision 2026-09-24, fixing the rc.3 observation that a revoked host agent
kept retrying its session a few times a minute. The controller now answers a revoked
certificate's session, and closes its live session, with a typed refusal
(`PermissionDenied`, exactly `host_certificate_revoked`), but only when the
certificate presented over mutual TLS is one it revoked (store
`certificate_revoked`, by fingerprint or with its host); every other failure stays the
generic refusal. The agent acts on that exact answer alone: `run_session*` return
`HostRevoked`, and `mllm start host` logs one line (`error [host_revoked]: ...`
naming `mllm invite host <id> --recover --output FILE` and
`mllm join host --join-file FILE --recover`) and exits with the new code 14
(`ExitCode::HOST_REVOKED`). Nothing is stopped or signalled, so engines stay for
`join --recover` to re-prove (ADR 0016, amended in Consequences). An unreachable or
restarting server, a version refusal, a generic or look-alike refusal, and an
impostor endpoint (certificate not from the pinned CA) keep the existing backoff.
Both host units add 14 to `RestartPreventExitStatus`; `scripts/verify-packaging.sh`
now checks 2, 3, 5 on every unit and 14 on the host units. The standalone role has no
enrolled host to revoke, so its units are unchanged. Exit codes are documented in
`docs/operations/install.md` ("Exit codes the units do not restart").

Tests (T05, T06): store `certificate_revoked_names_only_revoked_certificates`; agent
`only_the_exact_revocation_refusal_stops_reconnecting`; mTLS integration
`revocation_closes_the_session_and_refuses_reconnect_and_commands` (the agent now
returns `HostRevoked`), `only_the_controllers_exact_revocation_answer_stops_the_agent`,
`an_impostors_revocation_answer_never_reaches_the_agent`; CLI
`a_revoked_host_exits_with_its_own_code_and_the_recovery_commands`. With the old retry
behaviour restored, three of the integration tests fail. Local only: core 1007 passed;
workspace all-targets 1772 passed, 1 ignored (`--test-threads=4`; at the default
thread count four load-sensitive tests in unchanged code, `mllm-launchers` process
visibility and `native_vllm` readiness, failed once and pass on rerun); Clippy clean
with warnings denied; `scripts/verify-packaging.sh` passed (shellcheck not installed,
skipped); `scripts/test-install.sh` passed. CPU and mTLS tests are not qualification:
not live-proven. Pending: a live revoke on a host under the packaged unit (exit 14,
unit not restarted, engines alive), then `join --recover` re-proving them.

## SGLang saver observation failure at `receive_header` — 2026-09-25 (branch `fix/sglang-observation-failed`)

The rc.2 M28 sa-14 refusal (`park_refused` after `native_observation_failed` at
`receive_header`) is still **not root-caused**. This branch adds diagnostics only;
it changes no park, wake or observation outcome.

What the rc.2 evidence shows. The host journal on host-a has exactly two
lines at 19:32:56.146 local time (23:32:56.146Z): `native_observation_failed`
`receive_header`, then `saver_observation_unavailable` `observe`. There is no
`sglang_park_not_quiescent` line, so the quiescence read and the
`saver_mapped_before` read passed. The failure was the third saver read in the
park (the precondition read in `Run::run`), 2.53 s after the server accepted the
park. A `receive_header` failure means the socket gave no 4-byte header before
the 1500 ms budget ran out, or the engine closed the connection without a frame.
The bare stage could not tell these apart. The engine's own stderr goes to
`/dev/null` without `--debug-engine-logs`, and the rc.2 host directory has since
been removed.

Code finding (not a behaviour change). When the scheduler does not reach a safe
point in time, the engine never sends its `uncertain` frame. The bridge's slot
expires at the same deadline the transport sends by, and the host's budget starts
before the engine's. So on the host a slow scheduler always looks like
`receive_header`, never `response_status`. Tightening the budgets so an
`uncertain` frame always arrives in time is a possible follow-up. It is not in
this branch.

Diagnostics added:
- Host (`crates/mllm-launchers/src/native_observation.rs`): each socket-stage
  failure (`send`, `receive_header`, `receive_body`, `receive_eof`) logs one line
  with `cause` (`deadline`, `eof`, or `error` plus its errno), the bytes received
  of those expected, `elapsed_ms` against `timeout_ms`, and whether the enrolled
  scheduler is still alive (`owner_alive`). The send and trailing-EOF paths used
  to fail without any stage line. A success that took a third of its budget or
  more logs `native_observation_slow`, so near misses show up in the host
  journal without engine logs.
- Engine (`runtime/sglang_observation_transport.py`,
  `runtime/sglang_scheduler_observer.py`): each served connection logs one
  `mllm_observation_served` line. It carries the outcome, the stage where a
  connection without a frame stopped, and whether a frame went out. It also
  carries the time from accept to when the bridge was asked and answered, the
  bridge's wait for a safe point, the snapshot duration, safe points seen, lock
  contention, and whether the result reached the slot. This line is visible
  only with `--debug-engine-logs`.

Live (host-a, SGLang 0.5.20, qwen3-14b, fixture sa-14, run
`matrix-20260925T021319Z`; evidence `target/live/obsfail/`; the scratch loop row
is `target/live/obsfail/rows/OBS.sh`):

| Run | Result |
|---|---|
| `OBS-sa-14`, 20 park/wake cycles, engine logs off (the rc.2 setting) | 20 of 20 parked and 20 of 20 woke on request. 88.8–88.9% of the Ready drop was released each time. Wake took 185–225 s (disk reload). I1: one capture and 19 passes (max logprob delta 0.0). The same four engine identities stayed alive to the end, and cleanup was clean. **0** `native_observation_*` or `saver_observation_*` lines in the host log |
| `OBS-sa-14-dbg`, 10 cycles, `--debug-engine-logs` | 10 of 10 parked and 10 of 10 woke, with 88.8% released. All 160 served observations came back `observed`. Engine time from accept to close: p50 18 ms, p99 271 ms, max 288 ms. Wait for a safe point: 0 ms every time (the first tick). Snapshot: 13–15 ms. Contention: 0. The 100–290 ms outliers came from the transport thread's own work, not the scheduler. One cycle was high throughout. That fits GIL hand-offs against the busy-spinning scheduler, but it is not proven |

So the failure did not reproduce in 30 cycles, 0 of 30 (rc.2 1 of 3, rc.3 0 of 3).
The measured margin is 18 ms typical and 288 ms worst against a 1500 ms budget.
Hitting `receive_header` needs a stall more than five times the worst one seen.
Candidates the evidence cannot separate: a Python GC pause in the scheduler
(SGLang freezes GC only with CUDA graphs on, and the recipe has them off), a
procfs stall in the snapshot's `/proc/self/maps` read, or a GIL stall of the
transport thread. The new lines will name the stage, cause and timings the next
time it happens.

The live runs used the build before the last two diagnostic additions: the
host's `native_observation_slow` line and the engine line's
`request_ms`/`result_ms`. Those two are CPU-tested only. A third live run
stopped when Tailscale SSH asked for owner re-authentication on both hosts.
It was not retried.

Host state after the stop: both deployments in these runs were deleted. The
last cleanup check found no engine process, no GPU compute process and no
rendezvous directory on host-a. Still running or present, and needing a
reachable host to remove: the host roles in tmux (`mx-host-matrix-20260925T021319Z`
on both hosts) and the control-host server (`mx-srv-matrix-20260925T021319Z`). Also
present: `~/mllm-obsfail` (with `target/`) and `~/mllm-runs/matrix-20260925T021319Z`
on both hosts. The host-a tree was being overwritten by rsync when the SSH
check started, so its runtime files may be a mix of two snapshots. After
re-authentication, run `MLLM_MATRIX_LIVE=<repo>/target/live/obsfail
MLLM_REMOTE_TREE=$HOME/mllm-obsfail scripts/live/matrix/roles.sh down`
(this stops the hosts, then the server), then remove both trees and the run
directories on the hosts.

Local verification (CPU only, not qualification): the core suite passed 1004.
Workspace all-targets passed 1769, with 1 ignored. Clippy is clean with warnings
denied. The `runtime/tests` `test_sglang_*` suite is OK (181).

## Version skew policy and capability gating — 2026-09-24 (branch `feat/version-skew`)

Owner decision 2026-09-24: a SemVer skew policy between server and hosts (ADR 0017,
amending SPEC §13.1). The host sends its release version (`Connect.binary_version`,
field 7) and every post-baseline protocol feature it implements
(`Connect.capabilities`, field 8). Same `major.minor` line is supported; N-1 is
supported with `upgrade_recommended`; older, another major, or no/unparseable version
is drain-only (`upgrade_required`: only Inspect, Terminate, CloseIngress and Probe are
sent; not a placement candidate); a newer host is refused with "upgrade the server
first" and keeps reconnecting. Fourteen post-baseline features are catalogued
(`mllm_protocol::capabilities`); the send path refuses any command needing one the
host did not declare, typed and before anything is sent
(`host_capability_missing:<name>`, `host_upgrade_required`), launch/park/wake preflight
the same gate, placement requires `checkpoint_digest`, `startup_bytes` and
`restore_checkpoint_digest`, and a Terminate carries recorded identities only to a
host that declared them. `model_source_unsupported` became
`host_capability_missing:model_sources`. Versions and verdicts are recorded (schema
v34 `host_versions`) and shown in `list hosts` / `inspect host` and per allowed host
in deployment status. Upgrade order in `docs/operations/install.md`: server first,
then hosts one at a time. Consequence: hosts on releases before this one report no
version and are drain-only against an upgraded server until they are upgraded.

Local verification only: core 1070 reported, workspace 1764 passed (1 ignored), Clippy
clean with warnings denied across the workspace. CPU and mTLS transport tests are not
qualification; no mixed-version fleet has run on the hosts. Pending: a live rolling
upgrade (server first, then host-a, then host-b) once a release carries this change.
## Release candidate 0.1.0-rc.3 — build and live pass, 2026-09-24 (branch `docs/rc3-live`)

PR #13 (the rc.2 live findings) merged as 603ab7f and PR #14 (version bump,
plus the pre-release install docs and installer message) as 17469dd. Draft
pre-release `v0.1.0-rc.3` (unpublished; the owner publishes; the rc.2 draft is
untouched) targets `main` at 17469dd: `mllm-0.1.0-rc.3-linux-x86_64.tar.gz`
(control-host, sha256 `4853c80e…d370`), `mllm-0.1.0-rc.3-linux-aarch64.tar.gz` (built
natively on host-a, nice 19, sha256 `fb516655…20f0`), `install.sh`
(`43dd6183…2879`) and `SHA256SUMS`. Both passed `scripts/verify-packaging.sh`
on their own architecture; `BUILDINFO` commit 17469dd, not dirty, runtime
manifest `80044870…ddee0`.

Live pass with the installed release binaries only (`install.sh` from a
`file://` mirror of the draft, systemd user units, fresh state, no
`runtime_dir`). This time the host state used the documented default layout:
`~/.local/state/mllm/host`, with the host document at
`~/.config/mllm/host.yaml` (the unit's default `MLLM_CONFIG`, no env file).
Evidence: `target/live/rc3/`.

| Check | Live verdict |
|---|---|
| User state root (fix 1 of #13) | pass on both hosts: `~/.config/mllm` created first, no `~/.local/state/mllm`; `install.sh --systemd host` printed `created ~/.local/state/mllm (0700)`; after `init`, `join` and the unit's start it is still a real 0700 directory holding `host/`, `tmp/` and the engine runtime; no new "compatibility symlink" journal line |
| Refused park on a drain-only host (fix 2 of #13) | pass: host-b on rc.1 against the rc.3 server showed `upgrade_required`; vb-4 kept serving; `park deployment vb-4` was refused `park_refused` (`host_upgrade_required`, before any effect); the deployment was `reconciling` for one sample and `ready`, dispatch open, within about 2 s, serving 42, same engine PIDs and start ticks, no host session loss or agent restart. rc.2 reproduced a permanent 503 here |
| Upgrade host-b back to rc.3 | pass: `supported`, state root still a real directory |
| M73 va-4, sa-4 | pass: launched from `~/.local/state/mllm/host/runtime`, loopback only, unkeyed 401, stop and restart clean |
| M31 vb-4 ↔ sb-4, tight host-b, 1 cycle deep | pass: park and wake on the same processes |
| M28 sa-14, three runs | pass 3 of 3 (89% of the Ready drop released each time); `native_observation_failed` did not recur (0 lines in the host journal). The single rc.2 occurrence stays unexplained |

Local (CPU only) for #14: core 1004, workspace all-targets 1767, Clippy clean
with warnings denied, `scripts/test-install.sh` passed including the new
pre-release case. The CPU tests prove the fixes' logic; only the rows above
prove them on the hosts, and none of this qualifies an engine recipe beyond
the q4/q14 fixtures exercised.

After the pass every role and unit was stopped and uninstalled. Both hosts
have no engine, role or GPU compute process, no rendezvous directory, no
`~/.local/state/mllm`, `~/.config/mllm` or `~/mllm-rc3-*`; the server state
stays on control-host under `~/mllm-rc3-server` (invitation files removed).

## Release candidate 0.1.0-rc.2 — live validation, 2026-09-24 (branch `fix/rc2-live-findings`)

PR #10 (soak fixes and harness) and PR #11 (version skew, ADR 0017) merged,
then PR #12 bumped the version. Draft release `v0.1.0-rc.2` (pre-release,
unpublished) targets `main` at 45f91af: `mllm-0.1.0-rc.2-linux-x86_64.tar.gz`
(control-host, sha256 `e99f57d9…d0c`), `mllm-0.1.0-rc.2-linux-aarch64.tar.gz` (built
natively on host-a, nice 19, sha256 `f7a5d598…bc1`), `install.sh`
(`ba339733…404a`) and `SHA256SUMS`. Both passed `scripts/verify-packaging.sh` on
their own architecture; `BUILDINFO` commit 45f91af, not dirty, runtime manifest
`80044870…ddee0`.

Live, with release binaries only: `install.sh` from a `file://` mirror of the
draft assets on control-host (`--systemd server`) and both hosts (`--systemd host`),
fresh state under `~/mllm-rc2-*`, host documents without `runtime_dir` (the
managed runtime the binary writes to `<state_dir>/runtime`), roles run by the
installed systemd user units (`~/.config/mllm/<role>.env` naming the document).
No repository build or synced tree on the hosts; the harness ran from its
scripts only, through the new `MLLM_LOCAL_BIN`, `MLLM_REMOTE_BIN`,
`MLLM_*_RUN_ROOT` overrides and `gen_host_doc.py --managed-runtime`. Evidence:
`target/live/rc2/`.

| Row | Verdict |
|---|---|
| M75 (no engine, both hosts) | pass: `invalid_config` "no engine installation", exit 2, nothing left |
| M73 va-4, sa-4 | pass: both engines launched from the managed runtime (`~/mllm-rc2-host/host/runtime/vllm_entry.py`, `sglang_entry.py`), loopback-only, unkeyed engine calls 401, stop with verified cleanup, restart at a new binding |
| M08 | pass: router and host ingress serve no engine or control path (404), engines loopback only and refused from control-host, control routes 401 unkeyed, vLLM marked `exposed`/not production safe, SGLang not exposed |
| M29 va-4 (vLLM park/wake) | pass: 77% of the Ready drop released, same processes, wake on request |
| M28 sa-14 (SGLang park/wake) | pass on 2 of 3 runs (89% released). The first run's park was refused `park_refused` after the host's saver observation failed (`native_observation_failed` at `receive_header`); the engine kept serving (fail closed). Not reproduced; open |
| M31 vb-4 ↔ sb-4, tight host-b, 2 cycles deep | pass: switches park and wake the same processes, reservations settle |
| M64 (`delete --stop`, drain) | pass |
| TC sa-4 (`qwen25`), vb-4 (`hermes`) | pass: 4 of 4 each (vLLM named choice finishes `stop` with the tool call) |
| `systemctl --user restart mllm-host` | pass: same engine PIDs and start ticks, new agent PID, reconciled, serves |
| M45 revoke with va-4 Ready | pass: `engines: retained`, dispatch 503, reconnect refused, engine alive |
| Recovery (`invite host --recover`, `join host --recover`) | pass: same host id, same engine processes at generation 1 (re-proven, not relaunched), serves |
| Mixed version (host-b on rc.1, server rc.2) | `upgrade_required` with its reason; start refused; a Ready engine keeps serving; stop and drain work; reinstalling rc.2 gives `supported` and a start succeeds. Found bug 2 below |

Bugs found and fixed on `fix/rc2-live-findings` (CPU-verified with failing-first
regression tests; the fixes themselves have not run live):

1. **User units and systemd ≥ 254.** `~/.config/mllm` (where the user units read
   `<role>.env`) existed before the first start, so systemd 255 on the hosts
   made `~/.local/state/mllm` a compatibility symlink to it; the unit's state
   and `TMPDIR` landed in the configuration directory. `install.sh --systemd`
   (user scope) now creates an empty 0700 `~/.local/state/mllm` and warns about
   an existing link; `install.md` documents it; `scripts/test-install.sh`
   covers both.
2. **A park refused before sending closed dispatch for good.** On a drain-only
   host the park preflight refuses `host_upgrade_required`, the coordinator
   settles it leaving the remote launch's dispatch closed until a fresh probe
   reopens it, but the readiness proof was kept, so no probe was sent: vb-4
   stayed `reconciling` with dispatch closed (503) while its engine ran.
   `RemoteEngine::residency` now forgets the proof on every park refusal
   (host `unchanged`, preflight, gate refusal), so the supervisor re-probes.
   Test: `version_skew.rs` `a_park_refused_before_sending_forgets_readiness_so_a_probe_reopens_dispatch` (T16 T34).

Harness fix: `M27.sh` checked the tight policy on host-a regardless of the
fixture's host.

Local on `fix/rc2-live-findings`: core 1004, workspace all-targets 1767,
Clippy clean with warnings denied, `scripts/test-install.sh` passed. CPU and
Fake-engine tests are not qualification. After the run every role was stopped,
the units and binaries uninstalled, and both hosts left with no engine, role
or GPU compute process, no rendezvous directory and no `~/mllm-rc2-*` state;
the server state stays on control-host under `~/mllm-rc2-server`.

Observations, not changed: a start refused because the only allowed host is
drain-only reports `capacity_blocked` ("capacity is unavailable") although the
scheduler's diagnostic is `host_ineligible`; a revoked host agent keeps
retrying its session (a few refusals a minute) instead of exiting.

## Soak M48–M50 — 2026-09-24 (branch `test/soak-m48-m50`, stopped by the owner)

M48 is not passed: the owner stopped the soak after 119 walked steps, short of
the 200 the matrix asks for, and M50 was not run. Harness: `rows/M48.sh`,
`soak.py`, `rows/M49.sh` (see `scripts/live/matrix/README.md`). Seed 20260924.
Deployments: va-4, sa-14, sb-4, vb-4 and vb-14 (every engine launched with
its tool parser) and the two-instance replica route `qwen3-4b` (sa-4-rep);
host-b on the tight policy, so two of its three single-instance deployments
fit and a request for the third switches.

- Segment 1 (commit `323e80d` binary, runs `M48` and `M48-r69`): 84 steps. Two
  invariant hits, both harness false positives (a stopped replica instance was
  credited with the binding of its sibling placed on the same host). It found
  two product defects, both fixed with regression tests and a failing-first check:
  1. `081e849`: a first placement whose checkpoint did not measure to the
     declared `content_fingerprint` failed as "runtime ownership is uncertain";
     it is now refused `checkpoint_mismatch` before any effect, as a wake is.
     (Found because `e0.sh checkpoints` fed its payload digest, which is not the
     product's manifest digest, to fixtures; the harness no longer does.)
  2. `227feb9`: a request for an operator-stopped deployment made room by
     switching before the operator-stop refusal, so on the tight host it parked
     a Ready incumbent and was then refused 429 (steps 77 and 80). The refusal
     now runs before any switch round.
- M49 on segment 1: every deployment deleted with verified cleanup and no residue
  by id, empty ledger, both hosts clean, MemAvailable within 0.5 GiB of the
  pre-soak baseline, roles exited 0. Before that, a server and both host roles
  were SIGTERMed and restarted with engines retained: they re-attached, and the
  invariant check and I1 passed on the re-attached engines.
- Segment 2 (commit `3585adf` binary, runs `M48-final` and `M48-final-r9`):
  35 steps, no invariant violation, then stopped by the owner mid-step. M49 on
  it: same clean outcome (MemAvailable within 0.1 GiB, roles exit 0).
- Coverage over both segments: routed inference and streams, tool calls, operator
  start with and without `--evict`, stop, park, wake on request, request-driven
  switching on the tight host, count-only revisions, instance stop and start,
  delete `--stop` and redeploy, drain host, host agent SIGTERM and restart,
  engine SIGKILL and agent SIGSTOP/SIGCONT. Refusals seen and judged expected:
  requests for operator-stopped deployments (429; its code is
  `insufficient_resources`, which reads oddly for an operator stop), and an
  activation that does not fit the tight host without `--evict`.
- Observations, not changed: a second replica instance that cannot be placed
  without eviction waits `queued` until its deadline (about 15 minutes), also
  after `start --evict --wait` returned; vLLM q4 greedy output flips a near tie
  at token 10 once the probe prompt is prefix-cached, so the soak compares an
  8-token I1 prefix.
- Local: core 982 reported, workspace 1983 reported, Clippy clean with warnings
  denied. CPU and Fake-engine tests are not qualification.

Remaining: M48 needs a full ≥200-step walk on the final binary, then M49 and
M50 (M73 on both engines and M08). Both hosts were left with no engine, role
or GPU compute process and no rendezvous directory.

## Distribution: one binary, GitHub Releases, install.sh — 2026-09-24 (branch `feat/distribution`)

Owner decision 2026-09-24: one self-contained binary, GitHub Releases and
`install.sh`; Homebrew deferred. Verified locally only (CPU tests and a
file:// installer fixture; not qualification of any engine recipe).

1. The runtime helpers are embedded. `crates/mllm-agent/build.rs` compiles
   every `runtime/*.py` (not `runtime/tests`) into the binary with a SHA-256
   manifest; `embedded_runtime::materialize` writes them to the managed
   `<state_dir>/runtime` (0700, files 0600, marker `.mllm-managed-runtime`,
   staged and renamed into place). `mllm init host`, `mllm start host`
   (document without `runtime_dir`) and `mllm start standalone` (no
   `MLLM_RUNTIME_DIR`) materialize it; a different manifest refreshes it, a
   changed managed tree is restored with a warning, an unmarked directory is
   refused, and a declared `runtime_dir` / `MLLM_RUNTIME_DIR` is never written.
   The server has no runtime (it launches no engine). Standalone no longer
   falls back to the checkout's `runtime/`.
2. Releases. The workspace version is `0.1.0-rc.1`. `packaging/release.sh`
   ships `bin/mllm`, units and docs (no `runtime/`), records the runtime
   manifest in `BUILDINFO`, copies `install.sh` and writes the release
   `SHA256SUMS`; `--sums DIR` rewrites it after gathering both architectures.
   The units run `/usr/local/bin/mllm` (user: `~/.local/bin/mllm`) and the
   standalone units no longer set `MLLM_RUNTIME_DIR`.
3. `packaging/install.sh` (POSIX sh, shellcheck-clean): gh, GitHub API with
   `GITHUB_TOKEN`, public URL or `MLLM_INSTALL_BASE_URL`; verifies the tarball
   against `SHA256SUMS` and every file against the archive's own sums, refuses
   on mismatch; `--system`, `--systemd <role>` (installed, never enabled),
   `--version`, `--uninstall`. `scripts/test-install.sh` exercises it under
   sh, dash and `bash --posix`; `scripts/verify-packaging.sh` runs it against
   the built tarball.

Draft release `v0.1.0-rc.1` (pre-release, unpublished; owner reviews before
publishing) was rebuilt after PR #8 merged and targets `main` at 6d7bf36:
`mllm-0.1.0-rc.1-linux-x86_64.tar.gz` (built on control-host, sha256
`606f7702…dbc2`), `mllm-0.1.0-rc.1-linux-aarch64.tar.gz` (built natively on
host-a in `~/mllm-release-build`, nice 19, sha256 `26dda1d9…5783`),
`install.sh` (`e09a327f…e43b`) and `SHA256SUMS`. Both passed
`scripts/verify-packaging.sh` on their own architecture; both binaries carry
runtime manifest `80044870…ddee0` and `BUILDINFO` commit 6d7bf36, not dirty.
The API and `gh` download paths of `install.sh` resolve published releases
only, so they work once the draft is published.

Not established: no release binary has run a role on a host, and the matrix
harness still declares `runtime_dir` (synced tree), so the managed runtime has
not launched a live engine yet.

## Model sources — 2026-09-24 (branch `feat/model-sources`)

Declared `huggingface` and `http` model sources are materialized by the host into
`<store>/sources/...` through the additive `MaterializeSource` action (ADR 0008
amendment 2026-09-24): pinned revisions and digests only, host opt-in
(`model_sources`, denied by default), a store reservation against `max_bytes`
before any byte is written, per-file verification, atomic commit, then the WE3
digest. Activation waits (`model_source_pending`) and status shows per-host state
and bytes. `mllm prune sources` reclaims unreferenced copies explicitly. Local
verification only, against a fake hub and origin: core 992 reported, workspace
1741, Clippy clean (schema v33, `ExecuteMember` field 14). Pending: a live Hugging Face download on a host; standalone
support; disk in the server's placement plan. CPU and fake-origin tests are not
qualification.

## Revoked host recovery — 2026-09-24 (branch `feat/host-recovery`)

Owner decision 2026-09-24: a revoked host recovers by re-enrolling under the
**same** identity (ADR 0016, amending SPEC §4.1). Implemented on
`feat/host-recovery`, rebased on `origin/main` `4decb9e` (after PR #3 host
revocation, PR #4 packaging and PR #5 schema downgrade guard; store v32 lands
after the guard).

- `mllm invite host <name|id> --recover --output FILE`: an explicit, single-use
  recovery invitation, 15 minutes by default (at most one hour), bound to the
  revoked host's id and journaled (`host_recovery_invited`). A host that is not
  revoked is refused `host_not_revoked` (409); an unknown host is `not_found`.
  An ordinary invitation for a revoked name is still refused.
- `mllm join host --join-file FILE --recover`: the host keeps its state and
  journal (or starts from fresh identity files if they were lost), always
  generates a new key, and gets a new certificate for the same host id
  (`host_recovered`). An ordinary join refuses a recovery invitation and
  `--recover` refuses an ordinary one. A retained identity for another host or
  controller is refused, never adopted.
- Revocation is now per certificate as well as per host (store v32): the old
  certificate stays refused after recovery; certificates of hosts revoked before
  v32 are carried in as revoked.
- On reconnect the existing reconciliation runs: a still-owned Ready engine is
  re-proven by a fresh probe against its recorded identities before dispatch
  reopens (not relaunched). A host that lost its journal re-proves nothing; its
  engines stay closed and charged until an operator stop settles them on gone
  evidence. For that, a Terminate now carries the server's recorded process
  identities (additive protocol field); a host with no record of the launch only
  observes and reports them, never signals them, so the launch is never released
  while one is alive.

Tests (CPU and Fake-engine only; not qualification): `mllm-cli`
`host_recovery` drives the real server and host binaries over mutual TLS (deploy
a fake engine, revoke, dispatch closed and reconnect refused, recovery of an
active host refused, recover by name, join `--recover`, same host id, engine
re-proven and served without relaunch, old certificate refused, invitation
single-use; and a lost-journal variant with an expired invitation refused,
dispatch closed and accounting retained while the engine runs, and an operator
stop issued while the host was away completing only on gone evidence by
identity). Also store, controller (real mTLS session), agent journal and
enrollment, protocol, management and grammar tests (T05 T06 T33 T34).
Local verification after the rebase on `4decb9e`: core 985 reported (984
distinct), workspace all-targets 1710, all passing; Clippy clean with warnings
denied. The
lost-journal test was shown to fail (stop never settles) with the recorded
identities removed from the Terminate. One early run hit a transient
`revoke host` request-journal refusal that did not recur in five later runs.
Pending: live rows M45 (revocation) and a live recovery row on the hosts.

## Schema downgrade guard and standalone `--config` — 2026-09-24 (branch `fix/schema-guard-standalone-config`)

Closes the two gaps the packaging guide found, verified locally only (CPU
tests; not qualification of any engine recipe).

1. An older binary now refuses state written by a newer one (SPEC §13.2, T33).
   `migrations::apply` returns `StoreError::FromNewerVersion { found, supported }`
   when the recorded schema version exceeds the binary's latest, before writing
   anything; the host journal returns `JournalError::FromNewerVersion` the same
   way. Roles report `store_from_newer_version` with a restore-backup or
   use-newer-binary hint and exit 5, which the units do not restart. Guard logic
   only: no schema version was added or renumbered.
2. `mllm start standalone --config <file>` is implemented (SPEC §15.2, R13). The
   explicit document is the one honoured; a missing or invalid one refuses with
   exit 2 and is never replaced by the generated or implicit document. The state
   root still comes from `MLLM_STATE_DIR`, a pristine root gets its credentials
   once, and a served root that lost them refuses. The packaged units keep
   starting without `--config`; `docs/operations/install.md` documents the
   precedence and the drop-in for an explicit document.

## Service packaging — 2026-09-24 (branch `feat/service-packaging`)

SPEC §4.3 service definitions and an F5-direction release tarball, verified
locally only. `packaging/systemd/{system,user}/` hold server, host and
standalone units: `Type=simple` foreground, `KillMode=process` on host and
standalone so engines survive a restart and are re-attached (server:
`mixed`), no draining `ExecStop=`, `TimeoutStopSec=90s` (drain_timeout + 60s),
`Restart=on-failure` except exit codes 2, 3 and 5, `OOMPolicy=continue`, and
engine-compatible hardening (no `PrivateTmp`, `PrivateDevices` or syscall
filter on host and standalone). `packaging/release.sh` builds a stripped,
reproducible tarball of git-tracked files with an owner-only `runtime/`;
`scripts/verify-packaging.sh` checks the unit invariants, runs
`systemd-analyze verify`, builds the tarball twice and checks its entries,
modes and digests. Operator guide: `docs/operations/install.md`.

Not established: no unit has run on a host. Whether the host unit's
hardening lets vLLM and SGLang start, park and wake, and whether engines
survive `systemctl restart mllm-host` and are re-attached, needs a live run.
Found while writing the guide: an older binary did not refuse a state store
migrated by a newer one, and `mllm start standalone --config` was refused as
not implemented; both are closed on `fix/schema-guard-standalone-config`.
Rollback across a schema change still needs a state backup.

## Post-merge live smoke — 2026-09-24 (branch `fix/live-smoke-2026-09-24`)

PR #1 (`edurdias/mllm`) merged into `main` as `eb33deb` after local
verification (no CI minutes available). A live smoke on both hosts then passed
M75, M73 on both engines, M08, vLLM and SGLang park and wake (M29, M28), a
cross-engine switch with warm processes (M31), M47, M53, M65, M66, M54, a sustained
frozen-agent run (M58: 2400 of 2400 requests, 5.4 s suspension, no replay), M64 and
M36, plus vLLM tool calls with a tool parser. It found two defects, both fixed on
`fix/live-smoke-2026-09-24` (from `origin/main`):

1. An engine's invalid-request rejection became a 500 and held an uncertain lease.
   The owner decided on 2026-09-24 that a complete engine response with status 400,
   413 or 422 and a JSON body is completion evidence, so the client receives the
   engine's status and message (`engine_rejected`) and the lease closes, while every
   other status stays uncertain (SPEC §10 note).
2. SGLang tool calls failed at the router with a 500, streaming or not (the adapter
   always streams from the engine). The cause was not the tool-call index: SGLang
   0.5.20 serializes each tool-call delta through pydantic without dropping unset
   fields, so the delta carries `"role": null`, which the strict delta check read
   as a role other than `assistant`. Read-only inspection of the 0.5.20 source on
   host-a (`serving_chat._process_tool_call_stream`, `ToolCallItem.tool_index:
   int`) shows the index is always an integer, so index validation stays strict. A
   null role is now an absent role; a non-null role other than `assistant` is still
   uncertain, and a chunk's `"usage": null` no longer overwrites collected usage. The
   regression test replays SGLang's exact bytes (T19).

Tool calls need the engine's own tool parser, passed through
`engine_config.extra_args` with `accept_extra_args: true` (vLLM
`--enable-auto-tool-choice --tool-call-parser hermes`, SGLang
`--tool-call-parser qwen25`); mllm relays, never parses (SPEC §10 note).

Harness: a row that fails or exits early now deletes what it deployed
(`cleanup_failed_row`, `KEEP_FAILED=1` keeps it), so a failed deploy no longer
leaves a route that makes the next row fail `route_conflict`. M64 judges cleanup
after `delete --stop` by deployment id. M53 now expects the failed target's
restart to be refused `startup_requires_empty_host` and checks ledger residue by
id. New rows: `TC` (tool calls, named and auto, streamed and not), `REJ` (engine
rejections), `M58` (sustained frozen agent); `M31` takes `SWITCH_MEMORY_JSON` for
the q4 pair; `M08` records tool-call behaviour without a parser (not gating).

Live after the fixes (2026-09-24, run `matrix-20260924T185326Z`): TC on SGLang
sa-4 with `qwen25` returned `get_weather` tool calls for named and auto, streamed
and not (4 of 4, finish `tool_calls`, well-formed SSE); TC on vLLM vb-4 with
`hermes` 4 of 4; REJ 40 of 40 rejections relayed 400 `engine_rejected`, the
streamed rejection an `engine_rejected` error event, no lease held, and the route
then served; M08 passed. The failure-cleanup trap was exercised with a scratch row.
Both hosts were left with no engine or role process and no GPU compute process.
Local: core 974 reported (973 distinct), workspace 1678 (one run had a load-timed
failure in `a_success_resets_the_attempt_budget`, 0 of 30 in isolation), Clippy
clean, runtime Python 276. CPU and Fake-engine tests are not qualification.

Open: the keyed vLLM admin-key probe was not run because the permission classifier
refused an agent reading engine keys from process environments, even with the
owner's relayed approval.

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
not native qualification: no host run has exercised the two-key embedded guard.

## Status reasons, solo-first-start switching and launch-failure reasons — 2026-09-24 (uncommitted)

Three local fixes from the M53/M53D/M66 live findings. They were not run on the hosts.

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
- [ ] Implement and validate distributed group launch/accounting on both hosts.
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

Read-only multi-node preparation confirms both hosts are reachable and use
aarch64 unified-memory host / driver 580.173.02. host-b has the required checkpoint but no
SGLang environment was listed. Product `start server`, `start host`, enrollment and authenticated AgentControl
sessions now pass local binary tests. Remote engine execution and native
two-host validation remain pending.

Implementation follows
`docs/plans/2026-09-21-1831-feat-two-host-sglang-plan.md`.
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
succeeded on both hosts with no GPU compute process on either. The native remote
gate on host-a is therefore unblocked but not yet run: no source sync, host build,
server/host deployment or remote native launch has happened since. host-b
had no known matching SGLang 0.5.20 environment; on 2026-09-22 the owner granted
a scoped exception to create a clean SGLang 0.5.20 virtual environment on host-b
mirroring host-a (same source commit, same wheels), with no driver, system package or
reboot changes and existing vLLM environments left untouched. The owner also
directed that the old host-a standalone service, if still alive, be stopped through
the shipped CLI and that work proceed until blocked or a live milestone is proven.
Later on 2026-09-22 both hosts were synchronized to the current worktree and built
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
with single-rank recipes: one control-host control plane managing both hosts, each host
running multiple vLLM and SGLang single-rank deployments, serving, switching models
and parking as needed. The end goal is a two-host control plane supporting both
engines in all meaningful permutations, mapped by an explicit test matrix. Two-rank
(TP2) group work, previously U6/U7, is deferred until after that matrix passes; its
open design questions (residency, peer exposure, NCCL transport, rank readiness,
compensation, owner granularity, placement shape, rendezvous ports) are parked.
The test matrix is `docs/plans/2026-09-22-two-host-engine-matrix.md`
(scenarios M01–M72, gaps G01–G17 plus U5-G1…G4, decisions D1–D11) and the work
plan is `docs/plans/2026-09-22-two-host-control-plane-plan.md` (units
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

Phase B passed live on 2026-09-22 with one server on control-host controlling both hosts
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
render to their native flags. Every host runtime directory needs the new
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
a local Task 2 implementation report, which
AGENTS.md had excluded as the owner's, are leftovers from earlier work. The
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

Live M16 (per-model smoke) started 2026-09-23 on both hosts. Its first run found a
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
`source_revision` token; the probe's 120 s limit is unverified on a host; and the
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
combinations across both hosts (run `matrix-20260923T034935Z`, snapshot `f5d793ea`,
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
updated together on each host), standalone engine ports are configurable
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
driver reports paused segments as unmapped on unified-memory host, segment counts for the large
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
tracks model bytes on the unified-memory host, about 21 tokens/s for 4B, 8 for 14B, 4.4 for 27B
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
`target/live/u5-remote-host-a/`. The first live run found two product bugs, both
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

The D5 model set is in place on both hosts with identical payload SHA-256:
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
`target/live/recovery-host-a/`). A killed host agent made the router answer 503
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
The requested end-to-end two-host inference/recovery/cleanup test is distinct
from the broader F4 residency, switching and cache qualification. Those
capabilities remain unqualified until their own evidence is complete.
Plan review covered coherence, feasibility, scope, security and adversarial
assumptions; it corrected an overbroad completion condition that had made all
F4 cache and switching work a prerequisite for this task.

Read-only SHA-256 comparison on both hosts confirms identical checkpoint
configuration, weight index, all three safetensors shards and tokenizer files
under `~/models/qwen3-4b-instruct`. No model or engine was launched for that check.
Both hosts report 200 Gb/s on their two direct interfaces. Bidirectional ICMP
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
`target/live/20260921T213446Z/`, with details in `live-f2.md`. Private state
is retained on host-a at `$HOME/.tmphQyl3M`. This is not native
qualification. Final S3 review and S2 remain pending behind the live gate.

## Local SGLang launch validation — 2026-09-21

At HEAD `b9b33af`, the uncommitted launch fixes pass the local diagnostic:
standalone reaches the wrapper and reports `launch_failed`, with journal evidence
`sglang_startup_failed: artifact_mismatch`, using `/usr/bin/python3` and the stub
checkpoint. This replaces the prior never-armed failure in this local reproduction;
it does not demonstrate native model readiness. The diagnostic state is retained
in a machine-local temporary directory. Running it inside a sandboxed environment
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
  `docs/plans/2026-09-19-sglang-launch.md`, commits `393b197..17b6ffc`).
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
      `docs/runbooks/live-f2.md`). The coordinator directs a native
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

## Earlier multi-node constraints, updated for the current two-host work

These constraints were originally deferred beyond the standalone F2 recipe.
The owner's current two-host instruction and the plan linked above now govern
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
6. Hardware: host-a has a single unified-memory host, so no parallel topology can be verified
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

- **Live rows owed by the final review fix wave (2026-09-26):** DG6 upgrade
  from 0.1.0-rc.4 (policy migration), DG1 switching (observed-memory park or
  stop), DG3 SGLang parking (saver permission warning), and one unified
  switching row plus the unified standalone boot on each lab host (one
  crediting rule; GB10 UUID cross-check).

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
and 218 Python runtime tests; all16 launch-decoder tests also pass on isolated host
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
this work cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified local Task 2 report
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
