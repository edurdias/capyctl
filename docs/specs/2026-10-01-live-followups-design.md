# Live follow-ups from the TensorFold run — design

Date: 2026-10-01. Status: owner decisions taken in chat; this note records them for
review before planning. Target release 0.1.1, with the TensorFold engine.

The TensorFold live run on host B (ADR 0023, rows TF1–TF5) surfaced four problems
that are not TensorFold-specific. Each section states the root cause, the owner's
decision and the design. Root-cause evidence lives in the investigation notes for
this branch; file references below are to the current tree.

## 1. Hugging Face cache link chains

**Problem.** Recent `huggingface_hub` caches store a snapshot file as a link to a
blob that is itself a link (`snapshots/<rev>/f -> ../../blobs/<h> ->
../../blobs/<xx>/<sha256>`). The checkpoint walker follows one link and refuses a
link to a link as `unsafe_file` (`crates/capyctl-agent/src/checkpoint.rs`, the
`S_IFLNK` arm). The start then reports "the checkpoint does not match its recorded
digest", because the controller's checkpoint gate collapses every refusal into
that text (`crates/capyctl-controller/src/checkpoint_digests.rs`, `verified`).

**Decision.** Allow a bounded chain of links whose every hop stays inside the
host's model store. Amend ADR 0014 §7.

**Design.**
- The walker follows at most 8 hops. Each hop is resolved lexically against the
  directory that holds the current link, opened from the store descriptor with
  `O_NOFOLLOW`, and must pass the existing containment check. The walk ends at a
  regular file. A directory, an escape from the store, a loop or a ninth hop is
  `unsafe_file`.
- The manifest records the first link's path and the final file's identity, so
  plain files and one-hop links keep today's digests.
- The gate keeps the refusal reason: a checkpoint that cannot be measured reports
  "the checkpoint could not be measured (<reason>)". Only a real mismatch says
  "does not match its recorded digest".

## 2. `validate config` without `--host`

**Problem.** Offline validation checks only the schema and the declared timeouts
and startup. The schema treats `resources` as an open structure with no required
fields; the strict shape is decoded only during resolution against a host. A
deployment the server refuses therefore passes `validate config`, against SPEC
§15.3 ("unsatisfied required fields" are refused before side effects).

**Decision.** Check a declared `resources` block offline with the server's own
decoding, and refuse a TensorFold deployment that declares no `resources`.

**Design.**
- A new `capyctl_config::effective::validate_declared_resources` decodes
  `resources` through the same typed recipe used by resolution and runs the
  intrinsic recipe and device-claim checks against the declared top-level
  `devices`. `validate config` calls it beside the declared-startup check.
- When the deployment names the TensorFold engine family, a missing `resources`
  block is refused offline as it is at acceptance (ADR 0023 §4).
- The text output of an offline validation also names what still needs a host.

## 3. Removing the last engine profile

**Problem.** A role with no engine refuses to start (SPEC T02, live row M75), so
`engine remove` refuses to drop a role's last profile with the start-up message
("set CAPYCTL_VLLM_BIN ..."). With the role stopped, removal is refused as
`agent_unreachable`. The only way out was deleting `engines.yaml` by hand.

**Decision.** Allow an empty host. Amend SPEC (T02 and the start-up rule) and
ADR 0018.

**Design.**
- A host or standalone role starts with no engine profile. It publishes an empty
  profile list, accepts and keeps deployments, and places none until a profile
  exists. `capyctl status` and the start banner say the role has no engine and
  name `capyctl engine add`.
- `engine remove` may remove the last registered profile. Retirement follows the
  existing path: deployments on that profile drain and stop first.
- ADR 0018 §4 stays as it is: a published profile is not removed while the role is
  unreachable. Because a role can now start with no engine, the user starts it,
  removes the profile and stops it again.
- Live row M75 and the T02 text change from "refuses to boot" to "boots with no
  engine and places nothing".

## 4. Client hang-up mid-stream

**Problem.** When a client disconnects, the router keeps reading the engine's stream
until `[DONE]` so the request's in-flight charge closes on evidence (SPEC §10:
"client disconnect is not proof the engine stopped working"). No engine
acknowledges a cancel, so the engine's own terminator is the only evidence today.
The GPU generates up to `max_tokens` for nobody, and a following switch waits for
it (17.6 s in TF5).

**Decision.** Cancel upstream on hang-up for every engine. Amend SPEC §10.

**Design.**
- On a client hang-up the forwarder stops reading and closes the engine
  connection. vLLM, SGLang and TensorFold abort a request when its socket closes.
- The request's lease moves to `cancelling` and stays charged. It closes when the
  instance's adapter reports engine-wide quiescence: no running and no waiting
  requests (vLLM and SGLang from their metrics, TensorFold from `/health`
  `requests_running: 0` and `busy: false`). This is the cancellation
  acknowledgement SPEC §10 asks for.
- Park, idle park and switch drains wait for `cancelling` leases like in-flight
  ones. An instance that never reaches quiescence keeps the charge, which is
  today's conservative behaviour.
- Streams whose client stays connected are unchanged. No partial stream is ever
  replayed (T38).
- Live checks before this counts as done: TF5 re-run on TensorFold, and a
  hang-up followed by park on vLLM and on SGLang (both authorized venvs on the
  lab hosts).

## Testing

CPU and Fake-engine tests for each section, written to fail first, tagged with the
acceptance IDs they cover (T02, T17, T38, T41 and the ADR 0014 checkpoint rows).
The live checks in §4 and a re-run of TF2 with an unmodified Hugging Face cache
qualify the changes. CPU and Fake-engine tests are not qualification.
