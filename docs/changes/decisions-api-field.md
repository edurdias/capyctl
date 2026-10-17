# Status: Decision models, slice A: the `api` deployment field — 2026-10-17 (branch `feat/decisions-api-field`)

ADR 0030 (accepted, owner decisions 2026-10-17) §1 and §7, the part that
resolves or refuses a deployment's API. Spec §15.1 and §16 gain the field and
§20 gains T43; the readiness, routing and canary parts of ADR 0030 (§§2–5,
spec §6.1 and §10) are other slices.

- **`api: chat | decisions`** on a deployment, `chat` when omitted, in the
  strict schema. It is frozen in the revision, part of its recipe fingerprint
  and of the command fingerprint only when it is `decisions`, and round-trips
  through the frozen snapshot. Every existing revision keeps its fingerprint:
  the golden vLLM and SGLang revisions decode unchanged, and `api: chat`
  resolves to exactly the revision that omits it.
- **Resolution refusals**, all before anything is stored or launched
  (`capyctl_config::decisions`): an installation that cannot serve decision
  models is `capability_missing:decisions` (vLLM and TensorFold always;
  llama.cpp below 0.7.0 and SGLang below 0.5.22, read from the profile's
  registered `build_fingerprint`; a fingerprint that names no version is
  refused, so it fails closed). The server checks it against the profile the
  host published and the host checks it again at launch against the same
  profile. From the document alone, with the instance spec: a `topology` of
  any shape and a declared `deep` or `host_backed` residency are refused; an
  undeclared residency resolves `restart_only`. A stated parser
  (`engine_config.<engine>.tool_call_parser`/`reasoning_parser` other than
  `auto`, or the options in `extra_args`) is refused, and parser selection
  returns none for a decisions deployment (ADR 0024 applies to chat only).
- **Host capability `decisions`** (ADR 0017): every current agent declares it.
  At deploy, a host whose latest session did not declare it is recorded
  refused `host_capability_missing:decisions` (a single named host refuses the
  deploy); placement passes such a host over; and a launch, park or wake of a
  decisions deployment on a live session without it is refused typed before
  anything is sent.
- **Status:** `capyctl status deployment` shows an `API` line and `"api":
  "decisions"` in JSON for a decisions deployment; `validate config --host`
  shows `effective.api`. Both new codes exit 5 and are in the errors page.

Tests (T43, with T14, T34, T39): `capyctl-config` `tests/decisions.rs` (round
trip and default, fingerprints of the golden revisions unchanged, each engine
and version refused or accepted, topology and parking residency refused,
parsers refused and none chosen) and the version parser's unit test;
`capyctl-protocol` `decisions_gates_only_decision_deployments`;
`capyctl-store` `a_decisions_deployment_is_never_placed_on_a_host_without_the_capability`;
`capyctl-management` `a_host_without_decisions_never_takes_a_decisions_deployment`;
`capyctl-controller` `a_decisions_launch_needs_the_capability`; the CLI's exit
codes, errors page and status rendering. CPU and Fake-engine tests are not
qualification. No live check yet: a decisions deployment cannot become ready
until the readiness slice lands.

# Release note: Decision models

- **Deployments declare their API.** A deployment takes `api: decisions` to
  serve a decision model, or `api: chat` (the default). A decisions
  deployment runs on llama.cpp 0.7.0 or newer or SGLang 0.5.22 or newer
  (anything else is refused `capability_missing:decisions`), on one machine,
  without parking and without a tool-call or reasoning parser; a machine
  whose CapyCTL is too old is never chosen for it
  (`host_capability_missing:decisions`). See
  [Decision models](../guide/deploy.md#decision-models).
