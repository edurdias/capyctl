# ADR 0017 — Host/server version skew policy and capability gating

**Status:** Accepted (owner decision 2026-09-24).
**Amends:** `SPEC.md` §13.1 ("rejects ... incompatible protocol/schema versions"): it
defines which host and server releases are compatible, what an incompatible host may
still do, and how additive protocol fields reach older hosts.
**Related:** ADR 0003 (versioned gRPC over mutual TLS), ADR 0016 (recorded process
identities on Terminate), ADR 0008 (MaterializeSource), ADR 0014 §7 (checkpoint digests).

## Context

The control session carries one protocol version, `"2"` (`mllm_protocol::PROTOCOL_VERSION`),
and a peer on any other version is refused. The command encoding version stays `"1"`
(`COMMAND_ENCODING_VERSION`) so journaled command digests stay verifiable across agent
upgrades. Since version 2 was introduced, every change has been additive: new fields and
actions on new field numbers, with an absent field encoding exactly as before.

Two things were missing:

1. **Release compatibility.** Nothing said which server and host releases may work
   together. `docs/operations/install.md` said "upgrade the server and its hosts to the
   same release; no compatibility between different releases is asserted", which leaves a
   fleet in an undefined state for the whole of a rolling upgrade.
2. **Additive fields versus older hosts.** An older host decodes a command, silently drops
   the field it does not know, recomputes the payload digest over what it kept, and refuses
   the command as a digest mismatch. Only two features were negotiated
   (`Connect.heartbeats`, `Connect.model_sources`); the rest (recorded process identities
   on Terminate, the recorded digest on a wake, `startup_bytes`, `size_only`, a nonzero
   instance index, ...) were sent to every host.

## Decision

### 1. The host reports its release version

`Connect.binary_version` (field 7, additive) carries the agent's Cargo package version, a
strict SemVer 2.0 string (`mllm_protocol::version::BINARY_VERSION`). The server parses it
strictly: `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`, no leading `v`, no leading zeros, at
most 128 bytes.

### 2. The skew policy

The server judges the host's version against its own (`version::assess`):

| Host version, relative to the server | Verdict | Effect |
|---|---|---|
| Same `major.minor`; any patch, pre-release or build metadata | `supported` | Full service. |
| One minor release behind, same major (N-1) | `upgrade_recommended` | Full service; status advises upgrading the host. |
| Older than N-1, another major, missing or not strict SemVer | `upgrade_required` | Connected **drain-only**. |
| Newer than the server (any minor or major ahead) | `refused` | Session refused: "upgrade the server first". |

**Drain-only** means the server may still stop, drain, revoke, terminate, probe and
inspect what it already owns on the host: it sends only Inspect, Terminate, CloseIngress
and Probe (`capabilities::drain_only_permits`). It never places, starts, wakes, parks,
digests or materializes anything there. The host is not a placement candidate, its engines
that are already Ready keep serving, and its status shows `upgrade_required` with the
reason. A host that predates this policy reports no version and is drain-only until it is
upgraded.

A **refused** host's session ends with `FAILED_PRECONDITION` and the message
`host_version_newer_than_server: host X is newer than server Y; upgrade the server first`.
The host logs that sentence and keeps reconnecting with its usual backoff, so it connects
as soon as the server is upgraded. Its engines are untouched.

**Release rule (0.x included).** A change that affects the protocol or durable state ships
only in a minor (or major) release. A patch release never changes the protocol, which is
why the patch level plays no part in the policy.

### 3. Capability gating for every post-baseline feature

The baseline is protocol version 2 as introduced: the typed member actions (Prepare,
Launch, LaunchSingle with its original plan fields, Inspect, Terminate, CloseIngress,
Probe, Park, Restore), member results with residency evidence, load reports and member exit
reports. Every feature added since is named in `mllm_protocol::capabilities::CATALOGUE`:

| Capability | Direction | Carries |
|---|---|---|
| `heartbeats` | server to host | Heartbeat messages, SessionReady bounds (also `Connect.heartbeats`) |
| `model_sources` | server to host | the MaterializeSource action (also `Connect.model_sources`) |
| `checkpoint_digest` | server to host | the DigestCheckpoint action; `SingleLaunchPlan.checkpoint_digest` and `checkpoint_weights_bytes` |
| `checkpoint_size_only` | server to host | `DigestCheckpointRequest.size_only` |
| `startup_bytes` | server to host | `SingleLaunchPlan.startup_bytes` |
| `instance_index` | server to host | a nonzero `CommandIdentity.instance_index` |
| `restore_checkpoint_digest` | server to host | `ExecuteMember.restore_checkpoint_digest` |
| `terminate_recorded_processes` | server to host | `ExecuteMember.terminate_recorded_processes` |
| `launch_failure` | host to server | `MemberExecutionResult.launch_failure` |
| `policy_refusal` | host to server | `MemberExecutionResult.refused`, `IngressProvisioned.refused` |
| `load_latency` | host to server | `LoadSample.latency` |
| `host_draining` | host to server | HostDraining and its acknowledgement |
| `process_residency` | host to server | `DomainObservation.residents` |
| `installation_fingerprint` | host to server | the installation fields of `RuntimeProfileStatus` |

The agent declares every capability it implements in `Connect.capabilities` (field 8,
additive; at most 64 short lowercase names, else the session is refused). The two earlier
booleans count as declarations of the same names.

- **Server to host.** `capabilities::required` names the capabilities a command's wire
  form needs; an absent (default) field needs none. The single send path
  (`AgentSessions::dispatch_to`, and the private ingress provision) refuses a command a
  host may not take, before anything is sent, with a typed reason:
  `host_upgrade_required` (drain-only) or `host_capability_missing:<name>`. Because
  nothing was sent, the refusal is an answer without effect: the lifecycle settles it as a
  refusal (`RuntimeError::Refused`), not as an uncertain effect. A command that some
  earlier session already received is never answered by a later refusal; it stays
  unresolved until a result or its deadline, as before.
- **Preflight.** Launch, park and wake check the same gate before their first host
  request (source download, digest request, ingress key), so the refusal is typed from the
  start and no preliminary request is sent either.
- **Placement.** `capabilities::PLACEMENT_REQUIRED` (`checkpoint_digest`,
  `startup_bytes`, `restore_checkpoint_digest`) are needed by every launch and wake this
  server sends. A host missing one, or drain-only, is not eligible for placement; its view
  lists what it lacks (`capabilities_missing`). Per-deployment features are refused per
  operation: a remote model source needs `model_sources` (the refusal replaces the former
  `model_source_unsupported`), a second instance on one host needs `instance_index` (and
  the per-instance journal claim it already required).
- **Terminate stays possible.** A Terminate carries the server's recorded process
  identities (ADR 0016) only to a host that declared `terminate_recorded_processes`, judged
  on its live session or, offline, on the declaration the store recorded. Any other host is
  sent the baseline Terminate and acts on its own journal record, as before the field
  existed. The proof required of its answer is unchanged.
- **Host to server.** Nothing to gate: an older host never sends these, and absence already
  means "not reported". They are declared so status shows a host's feature set.
- **Digest compatibility is kept.** No field is renumbered, the command encoding version
  stays `"1"`, and an absent field encodes identically, so every journaled command keeps
  its digest.

### 4. Record and show

The server records each host's declared version, capabilities and verdict on every
connect (store schema v34, `host_versions`), including a refused newer host. It is status
evidence, never an authority: the live session's declaration gates commands. It appears in:

- `mllm list hosts` / `mllm inspect host`: `server_version` at the top; per host
  `binary_version`, `compatibility`, `compatibility_reason`, `capabilities`, and in the live
  `session` view also `capabilities_missing`;
- `mllm status deployment <id>`: each allowed host in `hosts[]` carries `binary_version`,
  `compatibility` and `compatibility_reason` when recorded.

### 5. Upgrade order

Upgrade the server first, then the hosts one at a time. A server upgrade leaves every host
N-1 (supported) or older (drain-only), never newer. A host upgraded before its server is
refused until the server catches up; its engines keep serving meanwhile.

## Consequences

- A host running a release that predates this policy (no `binary_version`) is drain-only
  against a server that has it. The first rolling upgrade to a release with this ADR
  therefore holds new placements on each host until that host is upgraded; existing Ready
  engines keep serving.
- A server that predates this ADR ignores the new Connect fields and does not refuse a newer
  host. The policy protects fleets from the first release that carries it onward.
- A Terminate built while a host lacked `terminate_recorded_processes` keeps its digest; if
  the host is upgraded while that same command is still being redelivered, the rebuilt
  command of a later cleanup attempt may differ from the one journaled. Such a cleanup stays
  unresolved (never released) until a fresh cleanup step settles it on evidence.
- A future additive field must add a catalogue entry, a `required` rule and a test in
  `crates/mllm-protocol/tests/version_skew.rs`; the agent then declares it automatically.

## Verification

CPU and transport tests only (`crates/mllm-protocol/tests/version_skew.rs`,
`crates/mllm-controller/tests/version_skew.rs`, `crates/mllm-management/tests/host_versions.rs`,
unit tests in `mllm-store` `host_versions`, `mllm-agent` `session` and `mllm-controller`
`remote_execution`). They do not qualify any engine recipe; no live mixed-version fleet has
been run.
