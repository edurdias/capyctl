# Runtime preflight

## Installation identity and capability probes

ADR 0008 (owner decision 2026-09-23) retired the pinned SGLang source audit
(`sglang_source_preflight.py`) and the pinned saver source inventory
(`saver_source_preflight.py`). No file of an engine installation is compared to
hard-coded hashes, and no permission rule applies to installation files, so a
custom, patched or group-writable build is not refused for being one.

Instead the host agent records each installation's fingerprint at registration
(the engine package's version and a `sha256:` digest over its files; see
`crates/mllm-agent/src/installation.rs`) and flags drift when a later launch
measures something else. Drift refuses a launch only under the installation's
host policy `security.installation_drift: refuse`; the default is `warn`.

`engine_capabilities.py` probes, by shape, the internals mllm hooks: the module
imports, the attribute or method is callable, the record declares the field,
the router serves the route, the metrics module names the gauge. Capabilities
are `core`, `deep_park`, `metrics` and (SGLang) `observation`. A missing one
refuses only the dependent feature with the closed category
`capability_missing:<name>`:

- `sglang_entry.py` probes after its guarded import: `core` for every launch,
  `deep_park` (saver adapter, importable saver, release, resume, reload-from-disk
  and flush routes) only when the memory saver is on, which is what `deep`
  residency renders. A `restart_only` launch serves on a build without the
  saver hooks.
- `vllm_entry.py` probes `deep_park` (sleep and middleware destinations, sleep,
  wake, collective RPC and prefix-cache routes) only when sleep mode is on.
- The host agent runs `python -I -S engine_capabilities.py <engine>
  <site-packages>` under the installation's own interpreter, bounded in time
  and output, before admitting a launch whose tier depends on a gated feature,
  and before a Park. The command prints one JSON report of missing probe
  labels per capability; it never prints paths or exception text.

Probes that pass are not evidence that a build serves a model or parks
correctly.

## Shipped inside the binary

Every `runtime/*.py` here (never `runtime/tests`) is compiled into the `mllm`
binary with a manifest of SHA-256 digests (`crates/mllm-agent/build.rs`;
SPEC §3.3, ADR 0001, owner decision 2026-09-24). A host without a declared
`runtime_dir`, and standalone without `MLLM_RUNTIME_DIR`, write them to the
managed `<state_dir>/runtime` at `init` and every start, refreshing it after an
upgrade and restoring it if it was changed; a directory without mllm's marker
is refused, never overwritten (`crates/mllm-agent/src/embedded_runtime.rs`).
A new module added here ships with the next build; an untracked one makes
`packaging/release.sh` refuse the tree as dirty.

## Owner-only rule for mllm's own helpers

`owner_only.py` mirrors `crates/mllm-adapters/src/owner_only.rs`: a file or
directory mllm put there itself is trusted when owned by root or the service
user, never writable by other, and group-writable only through the owning
user's private group. It governs this directory's modules, the protected entry
path, the reviewed saver library the scheduler binding reads, and the
observation listener's ancestor directories. mllm's private state stays strict:
the listener's own 0700 directory and 0600 socket admit no group write.

## Native startup output and external plugins

`sglang_device.collect_inventory()` reads a bounded local NVIDIA PCI/UUID
inventory from fixed Linux proc/sysfs roots without importing native libraries.
`observe_placement(spec, trusted_mapping)` recollects it twice and requires an
explicit service-authorized UUID/inventory digest plus that exact full UUID in
the inherited `CUDA_VISIBLE_DEVICES`. It never sets the environment or infers a
CUDA index from a logical device name. The inventory digest includes boot identity
and is separate from the policy's opaque hardware fingerprint. Trusted kernel
mounts and service-side mapping/namespace provisioning remain required.

`sglang_startup_guards` supplies narrow preimport helpers for a fresh, isolated
service-owned child only. `contain_startup_output()` permanently redirects stdout
and stderr to `/dev/null`, including C output and descendants. It is not a
context manager and must never run in the controlling service process. Report
only fixed exit/status categories externally; native output is discarded by default.

For development, the operator may explicitly run
`mllm start standalone --debug-engine-logs`. This enables native debug verbosity
and retains full output in the launcher's private engine log files. Those files
may contain credentials or other sensitive data; they are not copied into
management errors or journals. The flag is process-local and is not persisted.
Restart without it to restore output containment. Plugin, capability,
checkpoint, and placement checks remain enforced in either mode.

`enforce_closed_plugins()` rejects already imported `sglang`, `torch`,
`transformers`, or `torch_memory_saver` roots and submodules, nonempty
plugin/platform environment selectors, and installed entry points in either
SGLang plugin group without loading their targets. Run it before imports in
every spawned Python interpreter under trusted immutable package/metadata/search
paths. The no-site isolated launch disables automatic `site` initialization;
production composition still needs explicit verified package roots, without
executing `.pth` hooks or calling `site.main()`. An empty `SGLANG_PLUGINS` selector
is not an upstream disable switch. An empty no-site metadata search is not proof
that the eventual native package environment contains no plugins.

These helpers do not block explicitly opened file/network/terminal logs, attest
all imported code, or authorize native startup by their presence alone. Effective recipe checks, physical placement, worker enrollment,
and actual saver/allocation evidence remain independent obligations.

## Scheduler allocation observation transport

The observation path composes an enrolled scheduler's existing saver instance,
`sglang_scheduler_observer` safe-point bridge, `sglang_observation_transport`
framing/authentication, and `sglang_observation_server` protected Unix listener.
No helper imports an engine or converts allocation facts into Ready, idle,
release, residency, or qualification evidence; the host fuses them with its own.

`SchedulerObservationServer.start(...)` requires the exact current scheduler
identity and a separately enrolled live controller identity. It creates a new
socket only, under a canonical service-owned 0700 directory whose ancestors
follow the owner-only rule. Existing files/sockets are never adopted, replaced or repaired. The
socket is 0600, descriptors are non-inheritable, and one worker handles accepted
connections through one retained transport instance. The existing bounded replay
set therefore spans connections rather than resetting on each accept.

Retain the server for the scheduler lifetime. `close()` interrupts socket I/O and
waits at most three seconds for the worker. If a bridge or kernel call remains
stalled, close reports a generic error and retains custody until a later close
can confirm the thread ended. Successful close removes only the unchanged owned
socket; a replaced path is never unlinked. This is transport shutdown, not engine
cleanup or proof of memory release. Root/service UID and descriptor custody remain
trusted assumptions; the listener is not a sandbox against either.

Production enrollment (SPEC §9.2). A memory-saver launch whose host supplies a
private observation directory (`MLLM_OBSERVATION_DIR`, 0700) is started with
`sglang_observation_enrollment.run_enrolled_scheduler` as SGLang's scheduler
process target. In the scheduler process, after the Scheduler is built and
before its event loop, it installs the bridge with the SGLang 0.5.20 and
torch-memory-saver 0.0.10 reader (`sglang_saver_residency`), starts the listener
at `<dir>/<binding>.sock` in key mode, and writes `<dir>/<binding>.json` (0600):
binding, incarnation, the scheduler's process identity and the preload
library's path and digest. torch-memory-saver 0.0.10 exports no allocation
snapshot, so the reader takes the saver's per-tag MemPool segments and asks the
CUDA driver (`cuMemRetainAllocationHandle`, libcuda only if already loaded)
whether physical memory backs each one: a pause unmaps it, a resume maps it
again. Key mode replaces the enrolled controller PID with a per-launch key
derived from the admin credential, so a restarted host still observes the
launch it owns. Any enrollment failure leaves the engine serving without an
observation, and the host refuses Park unchanged. The host side is
`crates/mllm-agent/src/native_execution/saver_source.rs`.

The Rust `NativeObservationClient` interoperability fixture now uses this actual
listener and transport with synthetic saver facts in an isolated CPU Python
process, in both the enrolled-peer and key modes. Those fixtures use synthetic allocation observations; they do not load
a GPU or native engine. Current verification counts and installation limitations
are tracked in [F2 continuation status](../docs/runbooks/f2-current-status.md).

## Checkpoint identity

ADR 0014 §7 (WE3) retired the pinned checkpoint preflight
(`checkpoint_manifest.py`, `checkpoint_preflight.py`). Checkpoint identity is
now a `sha256:` digest over a canonical manifest of every file in the
checkpoint (relative path, size and SHA-256), measured in Rust by the host that
holds it (`crates/mllm-agent/src/checkpoint.rs`), recorded by the server when
the deployment is accepted, and re-verified by the host before every launch and
wake, for both engines. Nothing in this directory checks checkpoint files.

The descriptor-safe open chain the preflight introduced lives on in
`pinned_file_observation.py`, used only by the saver library binding
(`sglang_saver_binding.py`). Its failures expose the closed codes
`invalid_root`, `unsupported_platform`, `unsafe_file`, `artifact_missing`,
`artifact_changed`, `artifact_mismatch` and `io_error`.
