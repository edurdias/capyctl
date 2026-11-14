# Status: Outside GPU holders — 2026-11-13 (branch `feat/outside-gpu-processes`)

Owner decision 2026-11-13, ADR 0035 §8 and SPEC §7.2: outside GPU holders are bounded, read-only diagnostics on each GPU domain, carried by the optional host-to-server `outside_gpu_processes` capability, never required for placement. The web UI lists name (or `process`), PID and GiB under Other use, identifies another CapyCTL by its state directory or `unknown`, and reports the summed count and bytes of omitted holders. Rows are limited to 16 per GPU, names to 64 characters and state directories to 512 bytes. Collection does not read environments, expose arguments other than the state directory, or signal outside processes.

T16/T26/T29/T34 coverage includes fixture process trees, a CapyCTL ancestor two levels up, hidden processes, missing NVIDIA observations, nullable snapshot fields, optional version-skew behavior, bounded multi-GPU messages, and fit/start-refusal parity. The initial parity test exposed a missing digest prerequisite in its new fixture; recording that prerequisite made it pass. The touched Rust suites passed after rerunning four SQLite targets with `TMPDIR=/var/tmp/capyctl-webui` to avoid the initial tmpfs disk-I/O failures. Web lint, check, 233 tests, and build passed. A production-bundle layout fixture at 390px had no horizontal overflow and no page errors.

Rustfmt and clippy for the touched crates (`--all-targets --locked`, warnings denied) also passed.

The ignored, read-only live sampler test found PID 1094140 holding 13,553,893,376 bytes (12.6 GiB), identified another CapyCTL and its Shardio state directory, and also listed the demo's engine as another installation. Linux `comm` reports the scheduler's name as `sglang::schedul`; no engine arguments were used to expand it. CPU and Fake-engine tests are not qualification; this live check qualifies only the read-only observation, not a new engine recipe. No process was signalled for the live check and neither running installation was changed.

# Release note: Memory

- **See outside GPU holders.** Other use now lists outside processes by name, PID and GiB on hosts supporting the optional `outside_gpu_processes` capability. Another CapyCTL is identified by its state directory when available; long lists show the total for the remaining holders. This is read-only information, not a way to stop another process. Names and state-directory paths are visible to authenticated management clients and signed-in browsers; environments and other command arguments are not exposed.
