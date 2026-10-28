# Status: TensorFold native 1.x (ADR 0039) — 2026-10-28 (branch `feat/tensorfold-native`)

ADR 0039 (Accepted 2026-10-27; owner decisions of 2026-10-25 and 2026-10-27) is
implemented: TensorFold's native line, the Zig binary `bin/tensorfold-native` registered
from its release directory, runs beside the 0.6.x Python line under the same engine kind
`tensorfold`. The line is a property of the profile (`line: native`; absent means
`python`, so every existing profile and fingerprint is unchanged). No store migration.

What changed:

- Registration (§2): `engine detect` finds `tensorfold-native` on `PATH`, in `~/*/bin`,
  `/opt/*/bin`, `/usr/local/bin` and `--path`, version from `VERSION`, running nothing.
  `engine add` takes the binary, its `bin` or the release root; reads `tensorfold-native
  --version` on standard output and refuses a `VERSION` that disagrees
  (`engine_version_failed`); checks every `SHA256SUMS` line (`engine_unsupported` for a
  changed or missing file; a root without the list registers and `engine list` says so);
  reads `capabilities --json` (schema 1, `zig`, the checked version, `cuda`, every flag
  CapyCTL renders, `TENSORFOLD_CUDA_MEMORY_LIMIT_GB`) and records its families; measures
  the ADR 0008 digest over the whole root, cubins included. No toolchain check and no CUDA
  home. The profile is named `tensorfold` when free; otherwise `add` writes nothing and
  asks for `--name`. A `bin/tensorfold` outside a Python environment is refused. The role's
  own engine (`--tensorfold-bin`, `local_engine.tensorfold`, `CAPYCTL_TENSORFOLD_BIN`) takes
  a native path, the line following the file name.
- Certification (§3, §7, owner rule of 2026-10-27): 1.0.0 and 1.1.5 are the certified
  versions, each once its live rows pass (TF1–TF10, TF11–TF14); until then they, and every
  other native version, run as uncertified custom setups with a note in `engine add`,
  `engine list`, `validate config` and `status deployment`, never refused for their
  version. Only Nemotron 3.5 Lightning 30B-A3B 4-bit at revision `d9d758f` is a certified
  checkpoint; any other runs with a note, which adds that the engine is expected to refuse
  a family or format the installation does not list.
- Launch (§4, §5): `serve <model> --name --host 127.0.0.1 --port --no-update-check
  --backend cuda --snapshot-dir none --context --parallel [--max-tokens] [--thinking |
  --no-thinking]`, then host and extra arguments; never `--drafter`, `--kv-dtype` or
  `--idle-unload`. Reserved options (including 1.1.5's `--idle-unload`, `--allowed-host`,
  `--allowed-origin`, `--cortheon`, `--tp`, `--rank`, `--master`, `--master-port`) are
  refused by exact name and with `=value`, with no prefix rule. The closed environment
  holds the §5 names with `TENSORFOLD_MEMORY_RESERVE_GIB=2` and the system PATH alone;
  `TENSORFOLD_API_KEY`, `TENSORFOLD_REQUEST_LOG`, `TENSORFOLD_CUDA_KERNELS`,
  `TF_CUDA_DEVICE`, `TF_CUDA_SEGMENTS` and `BRAVE_SEARCH_API_KEY` are refused
  (`engine_env_reserved`).
- Resolution (§4, §6): `kv_cache_dtype`, `model.draft` and `--drafter` are
  `capability_missing`; a group is `group_shape_unsupported:tensorfold`, several GPUs or
  `topology: auto` `multi_gpu_unsupported`; `api: decisions` and parking residencies keep
  their refusals. The compaction options are ordinary, refused `retention_zero:compaction`
  under `retention: zero`, and `--record`, `--learn*` and `--recall*` are refused
  `retention_zero:engine_disk` there (exit 5), at resolution for a deployment's and at
  placement for a host's zero retention. The start bound is the ordinary weights-based one,
  with no 1800 s first-build term and no request-deadline floor.
- Running (§9–§14): readiness waits through refused connections and 1.1.5's 503
  `unavailable`/`draining` bodies, needs `/health` `status: ok` with the served name in
  `model_ids`, the name in `/v1/models` and the chat probe, and verifies `max_batch_size`
  against the rendered `--parallel` (`effective_args_mismatch`). The idle signal is
  `/metrics` `tensorfold:requests_running` and `tensorfold:requests_waiting` both 0, on the
  host's stop path and in load reports, which no longer fold `/health` for the native line
  (on hosts and embedded launches alike). The launch failure maps the engine's 1.0 and
  1.1.5 unserved-flag messages, its memory refusals and its unlisted-family refusal. Every
  native launch runs the loopback listener check after the probe and on each re-proof
  (`tensorfold_listener_exposed`). No extensions directory or lock cleanup for the native
  line.
- Placement (§15): a host declares `tensorfold_native`; a native deployment is refused
  `host_capability_missing:tensorfold_native` at deploy and placement on a host without it.
- Testkit: a native TensorFold HTTP fake (no listener until loaded, the 1.0 and 1.1.5
  `/health` and `/metrics` shapes, the engine's exact unserved-flag messages).
- Docs: SPEC §9.3, §10 and T41; ADR 0018 §1, 0023 §1–§8, 0025 §2, 0028 §10 and 0033 §10
  pointers; the guide (engines, engine options, requests, errors), configuration and data
  retention pages, `docs/examples/deployment-tensorfold-native.yaml`, and the approved
  environments in `AGENTS.md`.

Tests (tagged T41, with T14, T17, T21, T29 and T37 where they apply) cover registration
fixtures (version, `VERSION`, `SHA256SUMS`, the capabilities document, the digest, the
profile name), the rendered command and environment, every refusal, readiness, idle and
stop reads against fakes, load reports, the listener check and the capability gate; each
fails without the change it names. CPU and Fake-engine tests are not qualification: no
native binary was run and no lab host was reachable. The live rows TF1 to TF14 on host A
(ADR 0039, "Live qualification") are still needed; once TF1–TF10 and TF11–TF14 pass, their
entries in `tensorfold_native::CERTIFICATIONS` are set, and the native line becomes the
documented path for Nemotron 3.5 Lightning on a GB10.

# Release note: Engines

- **TensorFold native 1.x.** TensorFold's native binary registers from its release
  directory with `capyctl engine add ~/tensorfold-1.0.0` (no compiler needed), beside a
  TensorFold 0.6 profile; with a 0.6 profile already named `tensorfold`, pass `--name`.
  CapyCTL checks its version, the release's `SHA256SUMS` and its capabilities, starts it
  with exact-name option checks and its memory reserve set, waits for it to load, reads
  idle from its `/metrics`, and stops it if it listens off loopback. 1.0.0 and 1.1.5 with
  Nemotron 3.5 Lightning 30B-A3B on a GB10 are the combinations CapyCTL certifies once
  their live checks pass; other versions and checkpoints run as uncertified setups with a
  note. Under `retention: zero` its compaction and disk-writing options are refused
  ([TensorFold native](../guide/engines.md#tensorfold-native-1x)).
