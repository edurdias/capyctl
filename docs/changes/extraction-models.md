# Status: CapyCTL 0.2.0 integration and extraction capability — 2026-11-23 (branch `feat/extraction-models-0.2.0`)

The owner chose one combined 0.2.0 release-candidate pull request, preserving
published integration history and merging current `main`. It supersedes the
separate integration proposals for embeddings, LMCache defaults, tracing,
TensorRT-LLM and native TensorFold; their pull requests remain open for the
owner to decide. No release is published by this work.

## Extraction-capable engines (ADR 0041)

A deployment declares `api: extractions`; its frozen custom-engine manifest
requires a canonical non-empty `field_types` set from text, number, date, time,
datetime, duration, list, estimate and forecast. An engine or patched engine
serving `POST /v1/systemone` with `fields` can use this contract through custom
registration. Current built-in engine adapters do not implement that capability
and refuse extraction deployments. CapyCTL contains no model-specific runtime,
training recipe or model-project setup guide.

The deployment API selects the closed `questions` or `fields` contract.
Malformed requests, the wrong shape and undeclared field types are refused
before activation. Extraction launches run restart-only on one machine, without
a topology or parsers, with LMCache off, requiring both `custom_engines` and
`extractions` host capabilities. Discovery reports API and declared types.
Request/response bytes, request IDs, metrics, admission, concurrency and
uncertain-request accounting follow the existing inference path.

Readiness requires a fixed extraction for every declared type, not health
alone. The actual validated startup answer travels through the host's durable
launch result and is persisted before successful Initialize and dispatch. A
missing or malformed answer or failed reference write leaves dispatch closed.
A retained engine can recover its exact approved profile after an unrelated
profile publication and host restart, without adopting changed credentials or
safety policy. Completed-command compaction and verified stop retire the saved
policy; uncertainty does not.

A request may contain one named base64 PDF asset, not mixed with images or
other source text. CapyCTL does not render or decode the PDF. A successful PDF
answer must supply real `state_text`; text spans index its Unicode code points,
with optional one-based page metadata. Invalid evidence leaves the exchange
uncertain. Decoding, document limits and unsupported-input rejection belong to
the engine. No particular converter, OCR implementation or model is bundled.

**Input-capability gap:** manifests declare field types, not accepted media.
The router cannot reject images or PDFs before activation just because an
engine is text-only. That engine must reject unsupported input itself; the
fixed text-only readiness probe proves no image or PDF capability. A visual-PDF
evidence proposal and model-specific wrapper documentation are not included.

Historical T50 runs established routed plumbing but did not pass the complete
live gate. Startup-reference persistence and host reconnect fixes have CPU/Fake
regression coverage and still need live re-qualification. Model quality is not
a controller capability claim. CPU and Fake-engine tests are not qualification.

## Other inherited 0.2.0 capabilities

- **Embeddings (ADR 0036):** `api: embeddings` and `POST /v1/embeddings` on
  vLLM, SGLang, llama.cpp and matching custom engines, with streaming validation,
  typed readiness and wake references. Unsupported topology, parser, residency
  and LMCache combinations remain refused. Live rows EM1–EM9 remain required;
  no model certification is established by CPU checks.
- **LMCache defaults (ADR 0034):** automatic context-sized pools, bounded by
  host KV room, with shrink/off behavior, short-pool warnings and uncertified
  `required` setups. Parking residencies use engine-driven transfer; hosts
  declare `lmcache_engine_driven`. Qualification tables remain empty pending
  LC12–LC18; `restart_only` retains its prior transfer mode until its rows pass.
- **OpenTelemetry (ADR 0037):** role-local tracing, off by default, W3C context
  propagation, bounded OTLP export, lifecycle/ingress/request spans and optional
  supported-engine tracing on loopback. Headers and request/response content
  are not exposed as trace attributes. OT1–OT11 remain live gates.
- **TensorRT-LLM (ADR 0038):** environment registration with companion versions,
  one-GPU restart-only chat launches, owned configuration, KV validation and
  process/listener checks. Version lines 1.2, 1.3 pre-release and 1.4 release
  candidate remain uncertified pending their TR live rows. Profiles containing
  `companions` must be removed before downgrading to a pre-0.2.0 reader.
- **TensorFold native (ADR 0039):** native release-root registration alongside
  the Python line, capability/checksum checks, loopback launches, readiness,
  metrics-based idle checks and zero-retention restrictions. TF1–TF14 remain
  live gates; native versions/checkpoints are not certified by these tests.
- **Inference fixes:** router admission honors declared deployment concurrency;
  vLLM derived discrete-GPU requests no longer impose the historical 75% floor;
  SGLang supports independent prefill/decode CUDA graph policy and refuses
  editable installations through the capability-gated enrolled-host path.
  Native-engine confirmation of the documented live cases remains pending.

## Main integration and database safety

Main's host-administration cutover, browser UI/authentication, outside-GPU
process reporting and preview behavior are preserved. Custom field-type
confirmation and native/TensorRT registration now use the shared host handlers.
The tracing and outside-GPU protocol capabilities and their test coverage are
both retained; old command digests keep their encoding when new fields are absent.

Main's browser-auth schema v59 is unchanged. Embeddings, LMCache, tracing and
extraction migrations become v60–63 in order, including versioned migration
helpers. Earlier integration databases stamped v59–62 without main's auth tables
are refused before migration with an explicit, nonrestartable error.
**Lab-host warning:** preserve the old databases and stop roles only after
verified owned-engine cleanup. Use a separate state directory unless a deliberate
migration has been approved. Never reset version stamps or silently open an old
integration database under the new numbering.

Before downgrading below extraction support, remove extraction registrations:
older readers refuse a custom manifest containing `field_types`. No lab host,
model weights or protected evaluation partition is accessed by this integration.

## Verification

The focused store run, `cargo test -p capyctl-store --all-targets --locked --
--test-threads=4`, passed 570 tests across 21 suites, including preservation of
released v59 authentication and refusal of conflicting pre-release databases
without writes. Initial integration checks exposed stale registration paths,
router-auth composition and fixture signatures; the fixtures now use the
merged APIs rather than reintroducing old compatibility paths. Required fast
and deep `scripts/ci-local.sh` results are recorded in the review pull request.
CPU and Fake-engine checks do not replace the live gates described above.

# Release note: 0.2.0

- **Typed extraction as an engine capability.** Engines or patched engines
  registered with `api: extractions` answer typed `fields` at
  `POST /v1/systemone`, with declared field types, fixed readiness and durable
  startup evidence. Existing built-in adapters remain unsupported until they
  implement and qualify the contract. Text-layer PDF evidence is checked without
  converting documents in CapyCTL. Media support is engine-specific, not implied
  by readiness ([Extraction models](../guide/deploy.md#extraction-models),
  [Extraction requests](../guide/requests.md#extraction-requests)).
- **Embeddings.** Serve the OpenAI embeddings contract on supported engines,
  with typed readiness/wake checks and streaming answer validation. Native
  qualification remains pending.
- **Context-sized LMCache pools.** Automatic pools follow the model's context
  and available host KV room; uncertified required setups warn instead of being
  refused. Parking configurations use engine-driven transfer, with live
  certification still required.
- **Optional distributed tracing.** Configure a role's OTLP endpoint to trace
  request and lifecycle work, with optional supported-engine spans. Tracing is
  off by default; its engine options and environment names are now reserved.
- **More engine paths.** Register and run TensorRT-LLM for single-GPU,
  restart-only chat and TensorFold's native release alongside its Python line.
  These paths remain uncertified until their documented live rows pass.
- **Configured inference limits.** Router concurrency follows the deployment;
  small vLLM deployments can derive reservations below 75% of a discrete GPU;
  SGLang graph phases can be controlled independently, and upgraded hosts refuse
  editable SGLang before launch when the server accepts that refusal.
- **Upgrade safety.** Main's browser-auth migration remains v59 and the
  integrated schema advances to v63. Conflicting pre-release databases are
  refused with recovery guidance; preserve them and never edit their stamps.
