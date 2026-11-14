# Status: Extraction models: `api: extractions` and `fields` on `POST /v1/systemone` (ADR 0041) — 2026-11-11 (branch `feat/extraction-models-0.2.0`)

ADR 0041 (owner decisions 2026-11-04) adds a fourth deployment API,
`extractions`, with declared custom-engine `field_types`. It is part of the
frozen revision; manifests normalize and digest the declared set. Every native
engine is refused. Extractions run restart-only on one machine without a
topology or parsers, with LMCache off, and require both `custom_engines` and
`extractions` on the host.

`POST /v1/systemone` chooses the closed `questions` or `fields` contract from
the deployment API. The other API's key, whatever its value (both keys
included), and undeclared field types are refused before activation. Lookup
now precedes contract validation on this path, for decisions too: an unknown
model is reported before a malformed contract. Setting shapes are checked
on every type except scalar-only `null_threshold`, rejected on lists, estimates
and forecasts with 400 `invalid_request`; other meaning and agreement belong
to the engine. Responses use decisions' completion evidence and 16 MiB bound;
host ingress checks
its own 8 MiB request bound. Extractions use decisions' routing, admission,
leases, settlement, request IDs, trace context and metrics; requests cut
before a whole answer settle only at verified stop. Discovery lists
`capyctl.api: "extractions"` and `capyctl.field_types`. Readiness is a fixed
extraction of each declared type, accepting both published forecast forms;
the wake reference uses ADR 0041's typed comparisons and inclusive tolerances.
SPEC amendments cover §6.1, §10, §15.1/§16 and T50; deployment, request,
engine, error and operations pages document the contract.

Remote launches resolve the declared types from the frozen custom manifest and
require both host capabilities. The router retains its System One lookup through
admission instead of repeating it; chat and embeddings keep their existing
validation order. Extraction wake comparisons apply the decision canary's
inclusive tolerance with its `1e-9` rounding allowance, and readiness receipts
distinguish the host's report from evidence observed by the server.

Registration shows field types for administrator confirmation. Before
downgrading to a release without extraction support, remove every extraction
engine with `capyctl engine remove`. Older releases refuse the whole
`custom-engines.yaml` file if any manifest has `field_types`, preventing role
startup.

Verification on the 0.2.0 integration: `scripts/ci-local.sh` passed fmt, name,
clippy, unit, runtime, shellcheck, installer and changes.
`scripts/ci-local.sh --deep` passed those steps plus core and workspace.
The extraction router suite passes all 10 tests, including the added
raw-byte/numeric-spelling regression. CPU and Fake-engine tests are not
qualification.

A local smoke on a maintainer laptop (RTX 4090 Laptop GPU, isolated
standalone, `capyctl-extract` 0.1.0 from its own repository with a random
tiny Qwen3.5-architecture checkpoint on CPU torch): `engine add --custom`
showed the field types; the deployment became ready on the fixed extraction
of all nine types, and again after a stop and start; a declared subset
(`[number, text]`, stored as `text, number`) became ready on its smaller
probe; through the router the fixed request answered 200 with nine answers
and `capyctl.metrics`; `questions`, both keys and chat were `api_mismatch`,
an unknown setting `invalid_request`, an undeclared `date`
`field_type_unsupported`, an engine-side `min > max` `engine_rejected`, and
`/v1/models` listed `capyctl.api` and `capyctl.field_types`. A random
checkpoint proves the plumbing, not extraction quality. CPU tests and this
smoke are not qualification. The owner approved `capyctl-extract-4b`'s recipe
after E1–E10: frozen Qwen3.5-4B-Base, rank-64 attention LoRA, schema-first
layout, v7 span-set lists without refinement, exact computed scalars and
evidence-pointer distributions executed in the extraction server. ADR 0041's
2026-11-10 amendment records the two-seed 15M-token evidence, data v8, training
recipe and RTX PRO 6000/two-Spark hardware. These are pilot results; the
100M-token main run is launched separately and has no final results here.
The approved training environments on hosts A and B, including their installed
versions and flash-linear-attention, are recorded in AGENTS.md.

The owner selected 0.2.0: land `integration/0.2.0` first, then extraction
support with its embeddings/tracing integration retained. This branch is based
on `integration/0.2.0-on-0.1.11` at `7e0774d6` and replays only the nine extraction
commits above that base. Conflict resolution retains raw request/answer bytes
and declared concurrency policy from the newer integration line. Schema v62
follows its existing v61.

Live host-A qualification on 2026-11-10, using release binaries from
`035ce601`: **plumbing PASS (bf16), model quality FAIL for rc1; not a T50
release-gate pass**. The model server was repaired and reinstalled from
`capyctl-extract` commit `2aae3ea` after its original CPU-device default failed
the fixed extraction while `/health` stayed healthy. The repaired installation
was re-confirmed and pinned. All nine types, computed numbers, reference-date
offsets, qualified lists, comparable estimates, history forecasts and an image
returned complete routed answers with metrics through the remote host ingress.
Readiness, discovery, stopped-model pre-activation refusals, explicit restart,
engine-rejection completion and signal-only uncertain-request retention through
verified stop were exercised. No host-wide limits or owner jobs were changed.

The candidate's answers are not consistently plausible: quantity 3 × price 20
plus 10% tax returned 40 rather than 66; comparables 90–130 returned median 12;
a delivery datetime lost its clock time; a damage list omitted a stated item;
and the image amount/datetime were wrong. These failures reproduced with the
evaluated fp32 configuration under a larger per-deployment allocation, so they
are not attributable solely to bf16. The initial smaller-allocation fp32
SIGTERM's sender/cause was not established by the available event evidence.

Two conformance concerns were investigated. A Ready launch had no typed canary
row at the sampled point: the initial successful extraction answer is discarded,
then a second probe and reference write run best-effort after Ready with errors
ignored. Preserving and persisting the initial answer needs a cross-layer fix,
not a speculative live-session patch. Eventual persistence after the second
probe's full bound was not established. Separately, immediate lease closure on
a complete JSON 400 rejection is correct SPEC §10 behavior, not a defect;
ADR 0041 §3 now clarifies rejections versus 5xx engine errors. An own host-role
restart also left its online host's deployment reconciling after 120 seconds;
the retained charge settled on explicit verified stop.

The T50 controller fix now carries the validated startup extraction answer through
the adapter observation and the host's durable launch result. The controller
records that exact typed answer before completing Initialize or opening dispatch
(SPEC §6.1, ADR 0041 §§4–5). A missing or malformed answer, or a failed reference
write, fails the launch with a reason; extractions no longer run the optional
second readiness canary. T50 regressions cover the original answer surviving a
changed later answer, a second probe that would fail, reference-storage failure,
bounded protocol evidence and its durable host replay.

The reconnect investigation reproduced a retained-policy failure: a launch still
resolves while its approved host document is the in-memory previous publication,
but that document disappears when the host restarts with a newer publication.
Reproof then fails before sending the fixed extraction. The host now journals
the exact approved profile snapshot with the launch and can recover it for
retained-launch checks after restart (ADR 0018 §3, ADR 0031 §§5, 7). Recovery
requires the active profile and every non-profile host setting to remain
unchanged; an unknown or changed ownership, installation, key or safety policy
does not reopen ingress. T50 exercises the reopened host journal and ingress
against the Fake engine's `/v1/systemone` mode, including failed health,
malformed extraction and wrong-owner refusals. The saved live evidence does
not identify its failed reproof stage, so this reproduced cause is not claimed
as a confirmed diagnosis of that specific host-A attempt.

Before the fix, the first-answer regression failed because the successful
launch had no `wake_canaries` row. The retained-policy regression failed with
zero extraction requests after reopening the host journal, where one fixed
extraction was required. An unchanged-policy host restart already passed;
the failure was lost approved context, not a missing extraction branch.
Completed-command compaction removes the full saved policy payload, and a
verified stop evicts its in-memory copy; neither happens on uncertainty.

Verification for these fixes on 2026-11-11: both failing-before T50 regressions
pass after the fixes. `scripts/ci-local.sh` passed fmt, name, clippy, unit,
runtime, shellcheck, installer and changes. `scripts/ci-local.sh --deep`
passed all eight steps plus core and workspace. The read-only correctness and
security reviews found no remaining functional or security defect; the saved
policy payloads now follow completed-command compaction, and an unreadable
startup policy snapshot emits a bounded diagnostic while staying closed.

Both startup-reference persistence and host reconnect need live T50
re-qualification. No lab host was accessed for these fixes. CPU and Fake-engine
tests are not qualification, and rc1's model-quality failures remain unchanged.

The [live qualification record](../runbooks/f2-current-status.md#extraction-model-live-qualification-outcome--2026-11-10-branch-featextraction-models-020)
contains pass/fail tables, requests/answers, source and binary/checkpoint hashes,
defect locations/proposed fixes, and cleanup evidence. The final ledger had no
live resources or leases; every started process was stopped. Only the installed
release binary and authorized bf16 registration context remain on host A,
alongside the separately repaired approved serving environment. AGENTS.md now
records that environment as created, with its fingerprinted versions. The owner
will produce a new candidate in a quality phase and rerun T50; CPU and
Fake-engine tests are not qualification.

Table/record extraction is explicitly deferred to a follow-up release. The
proposed `records` contract, readiness implications, occurrence-conditioned
column readout, data, metrics and open questions are in
[Extraction records](../specs/extraction-records.md). This release retains ADR 0041's
nine field types with the pre-release wire amendments below.

The day-one contract now specifies scalar nulling at `p_null >= null_threshold`,
half-open Unicode-code-point spans, percentile 1–99 quantile arrays (one array
per forecast step), and `state_text` echo with unchanged `return_state_text`.
`mode` and `computation` stay internal as `raw['_mode']` and
`raw['_computation']`. Measured state/image/schema token usage aggregates
across prefix passes; schema accounting includes both copies, markers,
reference/control text and candidate suffixes, while `passes` counts full-prefix
field groups rather than cached suffix evaluations. The model's 8192-token
context reports truncation; PDFs and image text extraction are unsupported.
Image-only text and text-list fields are refused before inference with
`field_unsupported_input` by the model engine, not globally by the router.
Other image field types remain supported. The guide and ADR record deliberate
route, images/assets, options, string-error and 8 MiB boundary differences;
a future `/api/alpha/extractions` adapter is outside 0.2.0. Adapter and router
regressions cover non-scalar thresholds without tightening other irrelevant
settings.

The owner moved the model project to the private GitHub repository
[`ShardioAI/extract`](https://github.com/ShardioAI/extract), transferred from
`edurdias/capyctl-extract`. Its Hugging Face ID is `ShardioAI/extract-4b`, served
as `extract-4b` by `shardio-extract`; Python module commands use
`shardio_extract`. ADR 0041's ownership amendment supersedes the old names
without rewriting the smoke/qualification records above. Packaging defaults to
that Hub ID and never uploads; publication waits for the owner. Approved
host environment/run/source paths keep their original names. No repository or
Hub resource is created by this code change, and no release/version change is
implied. The guide and extraction probe/router fixtures use the new identities;
CPU and Fake-engine tests remain non-qualification.

Rename verification on 2026-11-12: `scripts/ci-local.sh` passed all eight fast
steps under its four-core/16 GiB limits; the extraction router suite passed
all 10 tests, including raw-byte model-ID rewriting from `ShardioAI/extract-4b`
to served name `extract-4b`. No live host was accessed for this rename.

## Private-package T50 re-qualification — 2026-11-14

The target is host A's private
`~/shardio-extract-latency/release-path/extract-4b-bf16-rebuilt-c4db434`,
served as `extract-4b`, with its standalone `extraction_model.py`, not the
previous rc1 or the serving environment's older installed model loader.
CapyCTL source is `302a42f`, including the startup-reference and retained-host
recovery fixes. This run uses its own work directory, state and free ports;
the owner's paused stack is not controlled or reconfigured.

### Exact live checklist

Each row requires captured request/response or lifecycle/ledger evidence.
An unexercised check is not a pass; CPU tests do not establish live qualification.

| Check | Procedure and pass criterion |
|---|---|
| Safety and provenance | Wait for GB10Runner; only owner EngineCore remains on GPU, at least 25 GB available before launch. Monitor owner process CPU in two consecutive samples, foreign GPU processes and available memory; stop on >20% core twice, a foreign GPU arrival or <8 GB available. Record source, binary and package hashes; check every selected port is free. |
| Plumbing and discovery | Register a pinned custom manifest with all nine canonical types; deploy through the shipped CapyCTL controller/router/host ingress, not direct engine HTTP. Verify Ready, public model rewriting, discovery API/types, metrics and completed lease settlement. |
| Manifest and deployment contract | Check canonical normalization with a text/number subset and undeclared date rejection before activation. Refuse invalid field types, native extraction engine, topology, parking residency and parser. Legacy fingerprints and old-host capability exclusions require separate coverage; never falsify a live host or rewrite a ledger. |
| Request contract before activation | While stopped, refuse empty fields, unknown setting/type, invalid name, missing list items, invalid reference date, other API's questions key (including null/both keys), wrong endpoint, and non-scalar null_threshold (list, estimate, forecast). Confirm no activation or lease. |
| Every field type | Route text with Unicode span validation; number including 3 × 20 plus 10% tax = 66; ISO date, clock time, datetime retaining clock, duration, text/number/date lists, bounded estimate and multi-step forecast. Compare factual answers with explicit input; report quality independently of wire validity. |
| Images and unsupported inputs | Route an image request for supported non-text types; image-only text and text-list must return engine rejection field_unsupported_input, not inference results. Mixed text/image text spans must index textual state. |
| State echo and internal members | return_state_text returns exact state_text (not state); no public mode/computation. Scalar value/guess obey p_null against explicit null_threshold. |
| Quantiles | include_quantiles produces 99 ordered values for estimate and 99 per forecast step, consistent with p10/median/p90 and declared bounds. |
| Token accounting | Capture measured state_tokens, image_tokens, schema_tokens, input_tokens, output_tokens and passes; images report nonzero visual tokens, generated output is zero and totals reflect full-prefix passes rather than fabricated counts. |
| Truncation | Send text over the 8192-token serving limit; verify explicit truncation reporting and bounded successful response or documented engine rejection, never silent truncation. |
| Startup canary | At the first Ready observation, read only the typed wake_canaries row and launch receipt; verify the actual nine-type startup answer is durable before dispatch, without a second best-effort canary. |
| Restart | Verified stop releases owned resources, endpoints and leases; start reaches Ready with a new startup reference and repeats the same deterministic text answer. Custom restart-only does not claim parking/wake or changed-answer injection. |
| Reconnect | Restart only this run's host role while engine is retained; require authenticated reproof, typed extraction, restored ingress and a successful routed request. Repeat after an unrelated approved-profile publication to exercise retained policy recovery. |
| Request-order independence | Capture fresh text baseline, run an image request, then repeat the identical text request several times; answers must match baseline exactly (rope-state regression), excluding timing metadata. |
| Rejection and uncertain settlement | Complete engine JSON 400 (min > max) closes its lease; interrupt a request only using identity-verified signals to this run's engine and confirm retention until verified stop. |
| Latency | Capture cold initialization and per-request wall/router/engine timings; repeated fixed text requests yield sample count, median, p90 and range, without comparing unmatched cache conditions. |
| Optional PDF dependency | Check approved environment without installing; if PDF libraries are absent, record finding and skip PDF checks rather than changing the environment. |
| Cleanup | Stop/delete this run's deployments and roles, verify no own GPU processes/listeners remain, and read-only ledger shows no active ownership/reservations/leases. Leave owner processes untouched and publish nothing. |

### Observed results

**Overall: FAIL for model quality; not a release-gate T50 pass.** The simple
numeric-list request below returns an empty list with high confidence, including
when explicit instructions are added. No CapyCTL implementation defect was
established by that response. The controller/router/host path passes the
exercised wire, readiness, typed-reference and retained-host-recovery checks.

The approved environment remains unchanged. Its older installed
`capyctl_extract.server` HTTP shell was used with a private launcher that loads
the package's standalone runtime and uses that runtime's request validators,
model and decoder. It does not load the older installed model implementation.
The launcher is pinned as a custom executable; this qualifies that exact
launcher/package combination, not a newly installed `shardio-extract` CLI.
Engine keys were neither read nor printed.

| Check | Result and non-secret evidence |
|---|---|
| Build/provenance | PASS. On A, Cargo 1.98.1 built `302a42f` with existing `~/.local/bin/protoc` 27.3: `PATH="$HOME/.local/bin:$PATH" ~/.cargo/bin/cargo build --release --locked --bin capyctl` in `~/shardio-extract-t50/source`. The initial build without that PATH failed for missing protoc; nothing was installed. Binary SHA-256 `3cf82b7bbc5b68542afb9df9c3f541ccca6f5d1dc087ebd20cf0bc5d28e86720`; package manifest SHA-256 `e6f2a2664ddf08a7933d127e9fc9be3adc40fbeb8aaa57ae6b3b0d03f338766a`; runtime SHA-256 `41ae28eb48c7c73e19e71ac1438bf0348caec69d443bb632cc04687c33b4c4fb`. |
| Isolation/safety | PASS for the exercised run. GB10Runner released A before GPU work. Initial available memory was 44,610 MiB; the final guard explicitly monitored the owner's container processes and EngineCore 1914989, using read-only host-journal and server-ledger PID/boot/start-time ownership. No actual foreign GPU process, owner CPU threshold breach or <8 GB memory event was observed. All listeners were checked free: management 47443, inference 48443, bootstrap/control 47444/47445, ingress 49443 and engine range 48100–48109, all loopback. |
| Checkpoint and plumbing | PASS. Measured checkpoint digest `sha256:bded05806ba1a8db2186b6fb21551d203c6fb3131c369db256f310aa7694876e`, weights 11,916,161,380 bytes. Ready with admission/dispatch enabled; `/v1/models` exposes `extractions` and all nine types. Routed responses carry public `model: extract-4b` and measured router metrics. The initial absolute model reference outside the configured model store failed closed with `invalid_root`; correcting this run's store root and relative reference resolved it without changing the package. |
| Manifest validation/normalization | PASS. `[number, text]` registers as `[text, number]`; empty, duplicate and unknown declared types each exit 27 `custom_manifest_invalid`, writing nothing. The stopped subset rejects date with HTTP 400 `field_type_unsupported` and remains stopped with admission/dispatch false. |
| Deployment refusals | PASS for `deep`, `host_backed`, topology and parser (exit 2, named reason). Native-profile attempt could not reach native capability resolution because no native profile was published (`profile_not_published`, exit 24): NOT EXERCISED for that requirement. No owner engine/profile was borrowed. |
| Request refusals | PASS. Empty fields, unknown setting/type, invalid name, missing list items and invalid reference date return 400 `invalid_request`; `questions:null`, both keys, chat and embeddings return 400 `api_mismatch`. List/estimate/forecast `null_threshold` each returns 400 `invalid_request`. Cold malformed requests leave desired state stopped, admission/dispatch false and no inference lease. |
| Text | PASS. `Customer Zoë García ordered blue chairs.` → `Zoë García`, span `[9,19]`, verified in Unicode code points. |
| Number | PASS for the tested computation. `Quantity 3 at price 20 each, plus 10% tax.` → 66.0. |
| Date/time/datetime/duration | PASS for the fixtures: `2026-11-20`, `14:30:00`, datetime retaining `T14:30:00`, and 3 hours. Startup probe also returns 11 days between its explicit dates. |
| Lists | Text/date PASS: all three damaged items and all three ISO dates. Number-list quality FAIL: `{"state":"Scores: 12, 25, 38.","fields":{"scores":{"type":"list","items":"number"}}}` → `value:[]`, confidence 0.9808759689. Adding `instructions:"List all scores"` still returns `[]`, confidence 0.9879462123. Both are complete HTTP 200 answers, not transport errors. |
| Estimate/forecast | PASS for the fixtures. Comparables 90,100,110,120,130 → median 110, p10 90, p90 130. Monthly history 10,20,30 → forecast 40,50,60. This is fixture evidence, not a general quality score. |
| Image | PASS. Generated 480×160 PNG with `TOTAL 66.00` → 66.0, HTTP 200, 654.306 ms wall. Mixed image/text preserves textual name/span. Image-only text and text-list both return 400 `engine_rejected`, message `text extraction requires non-empty state`. The router intentionally wraps the engine's rejection; its original error code is not exposed in that envelope. |
| Echo/nulling/internal members | PASS. Exact `state_text` echo, no response `state`; no public `mode` or `computation` in observed answers. Unknown delivery date with threshold 0.01 has p_null 0.9609517455 and value null. |
| Quantiles | PASS. Estimate has 99 quantiles; forecast has three ordered 99-element arrays, matching the corresponding constant step medians. |
| Usage | PASS for observed accounting consistency: text 55 = 8 state + 0 image + 47 schema; image 138 = 0 + 77 + 61; mixed 132 = 8 + 77 + 47. Every captured successful response reconciles input components, output_tokens is 0 and passes is 1. Exact processor tokenization and multi-prefix aggregation were not independently instrumented. |
| Truncation | PASS. Overlong repeated text returns HTTP 200, `truncated:true`, input_tokens 8192 = 8161 state + 31 schema, 2606.225 ms wall. Short requests report false. This tests reporting, not overlong-input answer quality. |
| Startup reference | PASS. First Ready read-only ledger has extraction/v1 canary, all nine canonical declared types and complete typed answers, same launch incarnation. The initial reference has amount 649, order A-88412, datetime `2026-09-28T14:30:00`, both damage spans and duration 11. No missing-canary regression reproduced. |
| Reconnect | PASS under the corrected harness: own host role restart retains engine PID 2851120/start tick 244268276, same incarnation and reservation, and serves the exact baseline answer (80.954 ms). |
| Retained profile recovery | PASS. After Ready, publishing unrelated `t50-unrelated` then restarting only the own host role again retains the same engine/canary and serves the exact baseline answer (81.693 ms), with admission/dispatch enabled. |
| Rope-state order independence | PASS. Fresh text and all three identical text requests after image/mixed-image requests have exactly equal complete answers, including confidence and p_null. |
| Latency | PASS for measurement. First successful cold initialization took 77.51 s including CLI overhead. Ten sequential warm Unicode-name text requests: wall median 61.626 ms, nearest-rank p90 62.316 ms, range 58.414–65.295 ms; 55 input tokens, one pass. No unsupported speedup comparison. |
| PDF | SKIPPED as authorized: approved environment lacks `pdfplumber` and `pdfminer` (also no pymupdf). No dependencies installed and no PDF request claimed qualified. |
| Unchanged fingerprints, old-host exclusion, comparison mismatch boundaries | NOT EXERCISED LIVE. No capability falsification, alternate old-host deployment, ledger write or changed-model injection. Restart-only custom engines do not support a parking wake; existing CPU tests are not relabelled live evidence. |
| Explicit restart | PASS. `stop deployment extract-4b` starts an asynchronous verified stop; `start deployment extract-4b --wait` waits for that stop, then reaches generation 4 with a new engine PID 2852128 and a new typed canary. Prior binding is released. Complete text answers equal the pre-stop baseline; restart response is 75.971 ms. The immediate post-stop snapshot is transitional, not falsely labelled fully released. |
| Complete rejection and uncertain request settlement | PASS for observed retention/closure. Normal complete JSON 400 rejection leaves no request lease. Identity-verified SIGSTOP to owned engine 2852128 followed by a two-second client timeout leaves an inflight request lease and the full reservation held. Identity-verified SIGKILL, stop and verified deletion then release the lease and reservation. The sampled lease was still inflight, so this does not claim the cancelling transition was sampled. |
| Final cleanup | PASS. Both run deployments deleted with `delete deployment … --stop`; final read-only ledger has empty request_leases, resource_owners, reservations and endpoint_leases, and every runtime binding released. Own roles 2851008/2851785 stopped; guard exits 0. All selected ports are free, no live owned engine identity remains, and nvidia-smi lists only unchanged owner EngineCore 1914989 at 73,592 MiB. Owner capyctl 1849524 and worker 1849416 remain; MemAvailable is 44,626 MiB. No host B access, installation, upload or publication occurred. |

The safety harness interrupted twice, without a genuine safety-limit violation.
Its first ancestry-only implementation called the retained orphaned engine
foreign after a host-role restart. Its second server-ledger-only implementation
missed an initializing engine, whose identity was already durable in the host
journal but not yet in the server's successful launch binding. Each interruption
stopped only this run's roles; orphaned engines were subsequently signalled only
after PID, boot ID and start ticks matched this run's read-only ownership
evidence. The owner explicitly approved resuming with the corrected union of
host-journal and server-ledger identities. These are harness defects, not
CapyCTL reconnect failures. The final guard includes all explicit owner-stack
PIDs and sums their CPU, a stricter condition than checking each separately.


Non-secret per-request and read-only-ledger evidence is retained on A at
`~/shardio-extract-t50/evidence/` and locally at
`target/live/t50-requalification/evidence/` in the qualification worktree.
Representative commands (each remote invocation used the required SSH
ControlPath and BatchMode options) were:

```sh
capyctl --state-dir ~/shardio-extract-t50/server \
  --config ~/shardio-extract-t50/server.yaml deploy model \
  --file ~/shardio-extract-t50/deployment.yaml
capyctl --state-dir ~/shardio-extract-t50/server \
  --config ~/shardio-extract-t50/server.yaml start deployment extract-4b --wait
~/capyctl-extract-serve-venv/bin/python ~/shardio-extract-t50/warm.py
python3 ~/shardio-extract-t50/restart_host.py
python3 ~/shardio-extract-t50/post_probe.py retained-policy-reconnect
python3 ~/shardio-extract-t50/run_fault.py
```

## Focused typed-list package recheck — 2026-11-14

The owner authorized a focused follow-up on the private
`extract-4b-bf16-rebuilt-9d9c5de` package (typed splitting of merged number/date
list spans), using the same CapyCTL `302a42f` binary and isolated controller/host
state, loopback ports and durable-identity guard. The exact recheck is:

1. `Scores: 12, 25, 38.` as `list`/`items:number`, with and without
   `instructions: List all scores`; require `[12,25,38]`.
2. One routed field of each of the nine types, including a three-date list;
   one image with total 66; then the exact baseline Unicode-text request,
   requiring complete answer equality.
3. One list `null_threshold` request must return 400 `invalid_request`.
4. Require Ready on the nine-type startup probe and read its persisted typed
   canary; capture ten sequential warm text latencies.
5. Delete the run deployment with verified stop, then stop own roles/guard;
   require empty accounting, no own GPU processes and all own ports free.

**Focused recheck: PASS for every requested check.** This closes the specific
numeric-list failure on the new package; it does not convert the earlier
package's historical FAIL or the unexercised full-T50 requirements into passes.
No CapyCTL code or approved serving environment changed.

| Check | Result and evidence |
|---|---|
| Provenance | PASS. Same `302a42f` binary SHA-256 `3cf82b7bbc5b68542afb9df9c3f541ccca6f5d1dc087ebd20cf0bc5d28e86720`; new runtime SHA-256 `0e224bb2dd289ff61130a4071d12a9fab668e32e09a804f599b488f41c10dc27`; manifest SHA-256 `90b438a207cc46be5e6e6ee3fe727e5013aa72c2733a7723e40c8ff3ce991f0e`. Registered `t50-9d9c5de`, using the unchanged pinned standalone-runtime launcher described above. Measured checkpoint digest `sha256:f565c2d79a69ff0b14796d9993d673e09f028ab85832a6f5d3924499932b4c79`, weights 11,916,161,380 bytes. |
| Scores without instructions | PASS. `Scores: 12, 25, 38.` with `{"type":"list","items":"number"}` returns HTTP 200, `[12.0,25.0,38.0]`, confidence 0.9808759689; wall 137.947 ms. |
| Scores with instructions | PASS. Adding `instructions:"List all scores"` returns HTTP 200, `[12.0,25.0,38.0]`, confidence 0.9879462123; wall 127.819 ms. |
| Text | PASS. `Zoë García`, Unicode span `[9,19]`, exact state_text echo. |
| Number | PASS. Quantity 3 × price 20 plus 10% tax returns 66.0. |
| Date | PASS. Delivery date returns `2026-11-20`. |
| Time | PASS. Meeting at 14:30 returns `14:30:00`. |
| Datetime | PASS. Delivery returns `2026-11-20T14:30:00`. |
| Duration | PASS. Three-hour repair returns 3.0 with unit hours. |
| List | PASS. All three dates returned: `2026-11-01`, `2026-11-08`, `2026-11-15`. |
| Estimate | PASS. Comparable prices 90–130 return median 110, p10 90 and p90 130. |
| Forecast | PASS. Monthly history 10,20,30 returns 40,50,60 for the next three steps. |
| Image | PASS. PNG `TOTAL 66.00` returns 66.0; 138 input tokens = 77 image + 61 schema, one pass, zero output tokens; wall 639.585 ms. |
| Order independence | PASS. Complete Unicode-text answer after that image equals the pre-image baseline exactly, including probabilities/confidence; post-image wall 73.561 ms. |
| Rejection | PASS. List `null_threshold:0.5` returns HTTP 400 `invalid_request`, naming `scores.null_threshold`; wall 6.041 ms. |
| Startup canary | PASS. Deploy/activate/wait reaches Ready, admission/dispatch true, in 89.76 s including process startup and checkpoint measurement. Before smoke requests, read-only ledger records extraction/v1, all nine canonical types and typed answers for new deployment `01M7E04H1D9GY7WQ0H07S0PG21`, incarnation `01M7E04H2AJATTCXPS49QBCVEB`; the old deployment's separate historical canary is not confused with this row. |
| Warm latency | PASS for measurement. Ten sequential identical text requests: wall median 61.671 ms, nearest-rank p90 63.591 ms, range 59.390–64.175 ms; 55 input tokens, one pass each. |
| Safety/cleanup | PASS. Only owner EngineCore was on GPU before launch; MemAvailable 45,698 MiB. All own ports checked free; durable-identity guard remained active without interruption and exited 0. Deployment deleted with verified stop; final read-only accounting has no leases, owners, reservations or endpoints and every binding released. Own engine PID 2904612 and role PIDs 2904442/2904467 are gone. Final nvidia-smi lists only owner EngineCore 1914989 at 73,592 MiB; owner capyctl/worker remain, MemAvailable 45,679 MiB. `ss -ltn` shows no own listener; all selected ports bind with SO_REUSEADDR. Initial plain-bind cleanup probes encountered only recently closed/TIME_WAIT sockets on 48443/48100, not surviving listeners. |

The focused requests and summary live on A in
`~/shardio-extract-t50/evidence-9d9c5de/`, copied locally to
`target/live/t50-requalification/evidence-9d9c5de/`. Startup/final read-only
ledger snapshots are `ledger-9d9c5de-{first-ready,final}.json` in A's
`~/shardio-extract-t50/evidence/` and the local
`target/live/t50-requalification/evidence/`. The request driver is
`~/shardio-extract-t50/focus_9d9c5de.py`, run with the approved serving Python.
Nothing was installed, published or uploaded; host B was not accessed.

# Release note: Extraction models

- **Typed extraction at `POST /v1/systemone`.** A deployment with
  `api: extractions` answers `fields`: text, numbers, dates, times, datetimes,
  durations, lists, estimates and forecasts, as declared by its custom-engine
  manifest. It uses the same endpoint and API key as decisions, with the
  deployment API choosing the contract. Unsupported types and the wrong shape
  are refused before waking a model. Only custom engines are accepted, on one
  machine, restart-only. Before downgrading to a release without extraction
  support, remove every extraction engine with `capyctl engine remove`;
  older releases refuse `custom-engines.yaml` containing `field_types`.
  This is part of 0.2.0; table/record extraction is deferred. The model recipe
  is selected, but the rc1 live run failed model quality despite passing the
  routed plumbing checks. A new candidate and T50 rerun remain release gates;
  CPU and Fake-engine tests are not qualification
  ([Extraction models](../guide/deploy.md#extraction-models),
  [Extraction requests](../guide/requests.md#extraction-requests)).
- **Extraction startup and retained-host recovery.** The typed canary reference is
  now the actual startup answer, persisted before a launch succeeds; persistence
  failure leaves dispatch closed. A retained engine can recover its exact approved
  profile after an unrelated profile publication and host restart, without
  adopting changed engine credentials or safety policy. Live T50 re-qualification
  remains pending.
