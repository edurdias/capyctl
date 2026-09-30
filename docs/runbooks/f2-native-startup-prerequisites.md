# F2 native startup prerequisites

Status: superseded on 2026-09-19. The entrypoint denial was composed open —
`sglang_entry._verified_native_contract` now runs the audited gates
(`sglang_native_composition.compose`: pinned-source revalidation, plugin
closure, placement attestation, checkpoint revalidation) and the guarded import
follows only when the contract holds (owner-authorized; S3 slice
`docs/plans/2026-09-19-sglang-launch.md`). The obligations below are
historical record of how the gates were built; the live-launch prerequisites
that remain are the host publishing `device_inventory_digest` and the guarded
launcher setting the child's `CUDA_VISIBLE_DEVICES`.

## Private launch scope

The trusted Rust handoff now emits private descriptor version 2. Its sealed
launch descriptor includes the current coordinator session, deployment revision
and generation, operation and step IDs, binding and incarnation, and the original
issued/deadline timestamps from the persisted Initialize execution. Those fields
do not enter public settings, argv, environment or diagnostic representations.

The Python decoder accepts explicit versions only. Version 2 requires the exact
scope shape, canonical identifiers, bounded integer timestamps and matching
public binding/incarnation. Version 1 remains shape-compatible with no scope;
it is never silently upgraded. The private immutable scope is metadata, not a
launch capability, enrollment proof, verified process identity or fresh clock
observation. Both unconditional native startup denials remain in place.

Focused Rust coverage first failed on the old version-1 envelope, and the new
Python positive case first failed because version 2 was unavailable. Final
verification passes all 15 runtime-binding tests, all 216 Python runtime tests,
and scoped Clippy. The isolated `949609b` helper install described below also
passes its 16 launch-decoder tests on Python 3.12.3. Earlier protected helper
installations predate version 2; none establishes a complete native startup
composition. Protected enrollment paths, child descriptor transfer,
complete process enrollment and startup integration remain unfinished.

## Remaining work

- Complete the closed native qualification program and both engines' persisted
  adapters. The current Store evaluator resolves only `CandidateLaunch::Fake`
  with `qualification-fake-v1`, two fixed markers and a 16-token output bound.
  The separate F2C marker helpers do not extend that authority or establish native
  case counts. vLLM still inherits the unsupported persisted-effect entry point;
  its legacy lifecycle implementation is not a permitted candidate shortcut.
  Existing vLLM environment files remain outside the authorized changes.
- The import-safe `sglang_server_args` mapper now checks 100 explicit native
  fields plus private/dynamic inputs and resolved graph backends after a guarded
  constructor call. Wire it only after the constructor's plugin, environment,
  logging, model/config, and GPU-discovery effects are guarded; remaining
  auto-resolved backend/page/chunk settings need effective-recipe checks.
  The frozen descriptor now carries the reviewed host ID,
  hardware fingerprint, logical device selector, and memory domain. Resolve and
  corroborate that selection against the observed physical GPU before any model
  load. The `sglang_device` collector now correlates bounded proc/sysfs UUID and
  PCI observations against an explicit trusted service mapping and a full-UUID
  inherited CUDA namespace. Provision/freeze that mapping against policy and
  establish the namespace before native imports; a logical selector is not a
  CUDA index or observed UUID by itself.
- The committed wrapper and local helpers are installed at the protected versioned
  path recorded below. Bind their identities and installed engine sources to the
  reviewed runtime recipe before use; installation alone grants no launch authority.
- Startup output containment and closed external-plugin checks are implemented
  and tested in CPU subprocesses. The protected entry script now repeats output
  containment and closed-plugin checks when CPython prepares it as `__mp_main__`,
  before unpickling the Process and its native argument classes. Four new CPU
  tests include real multiprocessing spawn and an import triggered by argument
  unpickling; a scheduler target-function guard alone would be too late. This
  requires retaining the protected script as the actual main path and using
  `spawn`; alternate main-module, fork and forkserver paths are not covered.
  The full local CPU runtime suite passes all 212 tests. The protected `c17819e`
  helper install on host-a passes all twelve startup-guard tests under its
  isolated Python 3.12.3, including real spawn. This does not authorize native startup.
  API preimport composition and enforcement of this native spawn topology remain
  open. Source preflight now covers both pinned
  plugin/platform initializers. Trusted package metadata/search paths and disabled
  alternate native logging channels remain required; these helpers are not a sandbox.
- Wire the production clock, checkpoint preflight, and credential resolver into
  `NativeCandidateService`; the current interface alone is not production composition.
  `OwnedCoordinatorState` now composes the lifetime lock before opening SQLite
  or starting a session. It derives fixed state paths, rejects unsafe existing
  database/sidecar files, and retains the lock beyond the connection lifetime.
  Seven ownership tests and fifteen runtime-binding tests pass. Wire this owner
  into the production worker at joint cutover; the legacy entrypoint is unchanged.
- Guard the actual memory-saver implementation, collect complete worker identities,
  and compare attributed allocations with the retained grant.
  `sglang_saver_binding` now checks the existing scheduler singleton chain,
  initialized pool, enrolled process identity, and mapped library backing-file
  provenance around one snapshot call. Its fourteen CPU tests pass. Attach the
  hook to the actual scheduler and install the reviewed binary before native use;
  these checks do not prove allocator routing or whole-process residency.
  The process-local scheduler bridge now observes after the original
  `process_input_requests` returns, with one retained request and bounded deadlines.
  Its thirteen CPU tests pass. The accepted-connection Unix transport now checks
  exact controller/scheduler process identities and kernel peer credentials, with
  1-KiB requests, 64-KiB responses, one active request, and a two-second socket
  deadline. Its twenty-one CPU tests pass. This is not a GPU synchronization point;
  the Rust client in `afe5b2b` now pins protected socket custody and exact scheduler
  peer identity, validates the closed response and same-namespace monotonic time,
  and requires terminal EOF within one deadline. All 39 launcher tests pass,
  including actual Python transport interoperability with synthetic saver facts.
  The scheduler-side listener now creates only a fresh protected 0600 Unix socket,
  retaining one thread and transport instance. Eight CPU tests cover custody and
  shutdown; all 208 runtime tests pass, and the Rust interoperability test uses
  this listener. Trusted path provisioning, startup attachment, complete enrollment,
  and durable consumption remain open. No transport result grants lifecycle authority.
- Typed single-effect controls are implemented in `6c78908`, with deterministic
  tests only; production observations and coordinator persistence remain open.
  Complete forwarding, coordinator/API/CLI integration, and F2C
  single-engine and mixed-engine live qualification on host-a.

These are implementation dependencies. No new owner decision is currently needed.
Existing authorization covers the isolated SGLang environment and reviewed observer
patch on host-a. Existing environments and drivers remain outside that change.

## Pinned source inspection

### Installed read-only preflight — September 15, 2026

On the authorized host-a, `git archive bf4b209 runtime` was extracted
into a new mode-0700 directory, `$HOME/capyctl-sglang-f2-runtime-bf4b209`.
The install refused an existing destination. No existing engine environment,
checkpoint, driver, or service was modified.

Using the isolated SGLang environment's Python with `-I -B`, the committed
helpers verified all nine selected installed sources and all ten checkpoint
artifacts (398 tensors, 8,044,936,192 payload bytes). External-plugin metadata
checks passed. The fixed proc/sysfs collector observed one physical GPU;
the boot-scoped inventory digest was
`2124d5550ed2316a62493cd335399bea795ffa074e07207c8b1d2f3a729387dd`.
An explicit module check confirmed no `sglang`, `torch`, or `transformers`
imports. There was no ServerArgs construction, native startup, or model load.

This is read-only prerequisite evidence, not device-policy provisioning,
whole-package attestation, live qualification, or durable launch authority.
The native entrypoint remains closed.

A second immutable helper install, `$HOME/capyctl-sglang-f2-runtime-8356c51`,
contains the committed saver-source and scheduler-bridge helpers. It used the same
new-directory/no-overwrite procedure. Import-free preflight verified and revalidated
all nine saver Python files against release `0.0.9.post1` and source archive SHA-256
`25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43`.
All nine selected SGLang sources and closed external-plugin checks also passed.
The check imported none of `sglang`, `torch`, `transformers`, or `torch_memory_saver`.
The observer binary patch is still not built/installed, and no model was loaded.

A third immutable helper install, `$HOME/capyctl-sglang-f2-runtime-c17819e`,
contains the spawn-preparation guard and prior committed runtime helpers. The
mode-0700 destination was checked absent and created without overwriting either
older installation. A read-only inspection of the isolated Python 3.12.3 confirms
that spawn prepares the main script before unpickling the Process. All twelve
startup-guard tests then passed on that interpreter, including real subprocess
argument imports and pre-import plugin rejection. No native package was imported,
no observer binary was built, no model was loaded, and no existing environment or
driver was changed. Full runtime verification on the local host passed 212 tests.

A fourth immutable helper install, `$HOME/capyctl-sglang-f2-runtime-949609b`,
contains the version-2 private launch scope. The destination was first checked
absent on host-a, then created with mode0700 and extracted from the committed
`949609b` runtime archive without overwriting prior installations. All sixteen
launch-decoder tests pass under the isolated Python 3.12.3 with `-I -B`.
An explicit loaded-module check confirms no SGLang, Torch, Transformers or saver
imports. The installed entry SHA-256 matches the committed source:
`570170d9509d3d5e954e0baba1a0c761166b1329c9fe82d3cde59ddd9db06d33`.
No existing engine environment, observer binary, checkpoint, driver or service was
modified. This verifies decoder compatibility only, not native startup or process
enrollment; both unconditional native denials remain closed.

### Interpreter startup hooks

The native renderer now uses combined `-IS` (isolated mode plus no automatic site
initialization). A CPU-only test with a fresh stdlib virtual environment proves
that `-I` alone still executes an installed `.pth` hook before protected entry
code; `-IS` prevents that hook. This is a controlled fixture, not evidence of an
unexpected hook in the installed engine environment. The renderer assertion was
RED on the old `-I` command before the change.

The actual multiprocessing-spawn test now uses the same flags and asserts both
remain enabled when deferred argument imports occur. All 13 startup-guard tests
are included in the passing 217-test Python runtime suite. All 10 renderer and
15 runtime-binding tests, plus scoped Clippy, pass. A read-only invocation of the
isolated host Python 3.12.3 confirms both flags and imports the existing protected
`949609b` entry helper without importing native packages. No environment or helper
file was changed by this check.

Disabling site processing also disables automatic package-path discovery. Complete
startup must explicitly establish trusted immutable package and metadata paths
before plugin inventory or native imports. It must not process `.pth` files or
call `site.main()` to recover those paths. An empty inventory under `-S` is not
installed-package attestation. That composition is still open, and both native
entrypoint denials remain closed.

The preimport guard now also rejects already-loaded Torch, Transformers and
torch-memory-saver modules, including their submodules. Checking only SGLang
missed dependencies that could have executed native effects earlier. Six sentinel
cases failed before the change and pass afterward without importing any native
package. All 14 startup-guard tests and the full 218-test local runtime suite pass.
The isolated host helper has not been updated with this change. Package-path
attestation and native startup composition remain open.

A disposable local interpreter check found that Python3.12.14 under `-IS` resolves
`sys.prefix` and `sysconfig` package paths against the base installation, unlike
Python3.14.7, which retains the venv prefix. Explicit package-root provenance is
therefore required; deriving it from the no-site child is not sufficient. The
matching read-only check on host Python3.12.3 could not run because one SSH
connection timed out on port22. This is not evidence about that interpreter's
current paths or a lasting host outage. No existing environment was changed.

### Source contract

The local `runtime/sglang_source_preflight.py` now verifies ten selected source
files and revalidates retained identities without importing the engine. Its
synthetic CPU tests and the full 213-test runtime suite pass. Production installed
root selection and consumption by the guarded startup still remain open; selected
files do not attest the complete import graph or compiled package.

The added [detokenizer source](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/managers/detokenizer_manager.py)
has SHA-256 `b8c8a453f34ef3b9777a763e911e9daab161b528a6fc123f42119cd2765afa1f`,
matching both the exact upstream revision and the isolated host installation.
A read-only invocation of the existing protected helper with the explicit
ten-file inventory verified and revalidated all selected installed files without
importing SGLang, Torch, Transformers or the saver. No helper/environment file
was changed on host. The new CPU test proves a missing or changed detokenizer
blocks the production-selected inventory. Process enrollment is still unfinished.

Source commit: `fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`. Inspection on September 14,
2026 did not import an engine or execute a model.

The fetched `server_args.py` SHA-256 is
`e04556de6d99ba8a76b91fffa49aa70ea9d65cd99f09d0da5d4cfe1228e28500`, matching the
earlier recorded comparison against the isolated installation on host-a.

[ServerArgs](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/server_args.py)
defines `tp_size`, `dp_size`, `tokenizer_worker_num`, `max_running_requests`,
`max_total_tokens`, and `mem_fraction_static`. Graph controls are
`disable_prefill_cuda_graph` and `disable_decode_cuda_graph`;
`disable_cuda_graph` exists internally but has no CLI option. Saver fields are
`enable_memory_saver` and `enable_weights_cpu_backup`. Device selection includes
`device`, `base_gpu_id`, and `gpu_id_step`. These findings identify fields, not a
complete approved mapping or effective allocation guarantee.

The remaining closed-feature fields located in that source include `dtype`,
`kv_cache_dtype`, `context_length`, `trust_remote_code`, `speculative_algorithm`,
`enable_lora`, `disaggregation_mode`, `enable_hierarchical_cache`,
`hicache_storage_backend`, `enable_lmcache`, `cpu_offload_gb`, `grpc_port`,
`grpc_mode`, and `smg_grpc_mode`. Singleton topology also needs explicit
`nnodes`, `node_rank`, `pp_size`, `ep_size`, `detokenizer_worker_num`, and
`use_ray` constraints. Automatic warmup (`skip_server_warmup`, `warmups`),
request logging (`log_requests`), compiler settings, and plugin discovery require
explicit treatment in the effective recipe. Do not infer their safety from TP=1.

[Saver adapter](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/utils/torch_memory_saver_adapter.py)
selects its real implementation when enabled, raises a recorded import failure when
enabled but unavailable, and selects its no-op implementation when disabled. The real
adapter's `enabled` property also checks the underlying saver. A configuration flag
alone cannot establish functioning memory release.

[HTTP startup](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/entrypoints/http_server.py)
accepts scheduler, detokenizer, tokenizer, and warmup hooks. Its server composition
installs the independent inference/admin authentication middleware. A protected health
gate must remain effective after that middleware is added, including OPTIONS.

The pinned `/flush_cache` handler returns a plain-text success response and status
200, or status 400 on failure. It does not return JSON null. Release and resume
handlers return implicitly on success (JSON null). Disk reload returns a JSON object
containing `success`, `message`, and `num_paused_requests`. Deterministic control
fixtures must reproduce these actual shapes before their tests can support native
integration.

[Engine startup](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/entrypoints/engine.py)
logs server arguments inside `_launch_subprocesses`, configures logging, and loads
plugins. Avoiding the public CLI does not by itself avoid that path. Startup must
have a tested secret-safe logging contract and a closed plugin policy. The source
also retains scheduler process handles and detokenizer PIDs; those are useful
enrollment inputs, but require independent start-identity corroboration.

## Verification prerequisite

The complete runtime suite needs `TMS_SOURCE_ARCHIVE` set to the existing pinned
archive, `<saver-source-archive>`. The suite validates its
SHA-256 before extraction. The initial unset-variable failure was resolved with
that local archive: all 97 runtime tests passed after the device-descriptor addition.
No owner action is required.
