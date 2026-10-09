# Status: llama.cpp engine designed (ADR 0029) — 2026-10-09 (branch `docs/llamacpp-engine-design`)

Owner decision 2026-10-09: llama.cpp's `llama-server` (v0.6.0, tag `v0.6.0`, commit
`d812350`) becomes the fourth engine, restart-only and one model per deployment for
v1, landing as TensorFold did (ADR 0023). Recorded as
[ADR 0029](../design/adr/0029-llamacpp-engine.md), with the design note
`docs/specs/2026-10-09-llamacpp-engine-design.md` and the plan
`docs/plans/2026-10-09-llamacpp-engine.md` (slices L1–L6). SPEC §1 R01 and §9.4
admit llama.cpp; ADR 0018 carries a pointer to the bare-binary registration.

Adopted defaults: a bare-binary installation (version read from `--version` on
standard error, `build_fingerprint` `<version>+<commit>`, digest over the binary and
the shared libraries beside it); qualification on a maintainer's discrete-GPU machine
first, a lab-host build only after owner approval; no engine key and a loopback
listener; `-c` rendered as the window times the slot count with `--no-kv-unified`,
4 slots by default, explicit `-ngl` and cache types, `--fit off`, `--cache-ram 0`;
`--fit*`, `--sleep-idle-seconds`, router-mode options, `--rpc`, `--slot-save-path`,
`--api-key*`, `--tools` and `--agent` reserved; `/etc/llama.cpp/config.ini` and
`LLAMA_ARG_*` closed as hidden inputs; router mode unused; speculative decoding
through approved extra arguments with the draft model counted (ADR 0014 A6); sizing
from the GGUF header with a placeholder margin until live rows; per-request figures
from `timings` and load from `/metrics` and `/slots`; `cache_salt` refused;
template-driven tool and reasoning parsing. A deep tier waits for an upstream
single-model sleep and wake.

The claims were checked against the v0.6.0 sources (`tools/server`, `common/arg.cpp`,
`common/common.*`, `src/llama-context.cpp`, `src/llama-kv-cache.cpp`, the build-info
CMake files and the release workflow) and a CPU-only build of the tag run for
`--version`, `--help` and parser behavior. No GPU run, no lab host. The design
note lists the ten items that need a live check. Documents only: no code, no tests.
CPU and Fake-engine tests are not qualification; the live rows LC1–LC6 are.

# Release note: none
