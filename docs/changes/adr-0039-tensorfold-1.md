# Status: ADR 0039 accepted: TensorFold native 1.x — 2026-10-27 (branch `docs/adr-tensorfold-1`)

ADR 0039 (Accepted 2026-10-27; target 0.2.0) designs TensorFold's native line, the Zig
binary registered from its release directory beside the TensorFold 0.6.x line, which stays
supported. The owner answered all eight open questions on 2026-10-27, and the ADR records
them. Decisions that changed the design: TensorFold 1.0.0 (at `~/tensorfold-1.0.0`) and
1.1.5 (approved for install on host A at `~/tensorfold-1.1.5`) are both certified, and every
other native version runs as an uncertified custom setup, never refused for its version;
1.1.5 was read at v1.1.5 (commit `84e0d56`), and its `/health` 503 `unavailable` and
`draining` bodies, its 20 s stop drain and `--idle-unload` are designed in, with
`--idle-unload` reserved because the idle policy is CapyCTL's; no checkpoint is refused for
being uncertified (only the pinned Nemotron 3.5 Lightning revision is certified, others run
with a status note, and the engine itself refuses a family it cannot load); `--compact-at`,
`--compact-keep` and `--compact-memory` are ordinary options, refused
`retention_zero:compaction` under `retention: zero`, and the disk-writing options are refused
`retention_zero:engine_disk` there; the default profile name is `tensorfold` when free, else
the operator passes `--name`. As recommended: `TENSORFOLD_MEMORY_RESERVE_GIB=2` rendered, the
loopback listener check on every native launch, the native line documented for Nemotron 3.5
Lightning on a GB10 once the TF rows pass, host A only. Live rows TF11 to TF14 are added for
1.1.5. Nothing is implemented; no code or tests change. CPU and Fake-engine tests are not
qualification, and the TF rows are the live check still needed.

# Release note: none
