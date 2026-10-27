# Status: ADR 0039 proposed: TensorFold 1.0 — 2026-10-25 (branch `docs/adr-tensorfold-1`)

ADR 0039 (Proposed, for the owner to decide; target 0.2.0) designs TensorFold 1.0.0, the
native Zig binary installed at `~/tensorfold-1.0.0` on host A (owner decision 2026-10-25),
beside the TensorFold 0.6.x line, which stays supported. It was written from the v1.0.0
source (commit `8b00301`), the Linux aarch64 release archive (read, not run) and the Nemotron
3.5 Lightning checkpoint at revision `d9d758f`. Findings that drive the design: 1.0's
`/health` has no `ok`, `busy` or `requests_running`, so today's readiness and idle reads fail
on it; it refuses `--drafter` and `--kv-dtype`, which CapyCTL renders or allows for 0.6; it
parses exact names only; it needs no build toolchain; it admits every stream at its full
window at start and refuses a launch that does not fit; on CUDA it serves only Nemotron-H in
MLX 4-bit, group 64; it has no sleep, unload or abort route and no release check; 1.1.5
already changes `/health` and adds engine-side unloading. Decisions proposed: one engine kind
`tensorfold` with a `native` line recorded on the profile (not a version comparison);
registration from the release directory (version on standard output, the archive's
`SHA256SUMS`, the `capabilities --json` document, a digest over the whole root including the
kernel set, default profile `tensorfold-native`); 1.0.x patches `custom`, 1.1 and later
refused; the native command and closed environment, with compaction to disk reserved;
refusals of `kv_cache_dtype`, drafters, topologies, decisions and unlisted families;
`restart_only`; readiness from `/health` `status` and `model_ids` with `max_batch_size`
verified; the idle signal from `/metrics` `tensorfold:requests_running` and
`tensorfold:requests_waiting`; no `/health` fold in load reports; cancelled requests on ADR
0032's existing rule; LMCache off; the loopback listener check; a `tensorfold_native` host
capability. The ADR gives the evidence with sources, the memory figures computed from the
source (17.25 GiB of weights, about 0.29 GiB a stream at 32,768 tokens), the implementation
plan, T41 extensions, live rows TF1–TF10 on host A and eight open questions with a
recommendation each. Nothing is implemented; no code or tests change. CPU and Fake-engine
tests are not qualification, and the TF rows are the live check still needed.

# Release note: none
