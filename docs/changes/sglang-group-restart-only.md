# Status: SGLang groups restart instead of parking — 2026-10-15 (branch `fix/sglang-group-restart-only`)

Found live at MN4 (SGLang 0.5.21, TP2 across two GB10 hosts): a group deep park completes, but the first wake fails. The head reloaded weights from disk for 222 s, the wake canary ran, and 3 s later the wake went `restore_uncertain` and the group was stopped. It fails safe, but group wake is not usable. vLLM groups pass 5/5 deep park cycles. Owner decision 2026-10-15, recorded as ADR 0028's note of that date.

- `group_support("sglang").deep_park` is now false (`crates/capyctl-config/src/group_support.rs`). `check_engine_shape` refuses every parking tier (`deep` and `host_backed`) for an engine whose group cannot park, with `capability_missing:deep_park: a sglang group cannot park (residency `deep`); declare `residency: restart_only``.
- `resolve_effective_with_checkpoint` (`crates/capyctl-config/src/effective.rs`) defaults the residency of a member of a group of more than one host to `restart_only` when its engine's group cannot park (the group's shape, else the member's `member_of`).
- Single-host SGLang and vLLM groups are unchanged; single-host identities are untouched.
- `docs/guide/engines.md` (groups table, known issue) and `docs/guide/several-machines.md` (park and wake) say SGLang groups restart instead of parking in 0.1.3.

Tests (T03, T22), in `crates/capyctl-config/tests/topology.rs`: `sglang_group_resolves_restart_only_and_refuses_parking` and `engine_shape_support` failed before; `vllm_group_resolves_with_default_residency` (now asserting `deep`) and `single_host_sglang_still_parks` pass before and after. Controller group world (`crates/capyctl-controller/src/coordinator/tests_groups.rs`): SGLang groups deploy `restart_only`; `sglang_deep_group_with_differing_paths_is_refused` now expects the deploy refusal; the three deep group park tests run on vLLM only. CPU tests only; not qualification. Live recheck: redeploy the MN4 SGLang group with no residency and confirm `restart_only` in status and that a park stops and a wake starts the group; with `residency: deep` confirm the deploy refusal.

# Release note: Multi-node groups

- **SGLang groups restart instead of parking (known issue).** In 0.1.3 an SGLang group across machines gets `restart_only` by default, and a group that states `residency: deep` or `host_backed` is refused at deploy with `capability_missing:deep_park` and a hint to declare `restart_only`: a live SGLang 0.5.21 group's first wake failed after reloading its weights. vLLM groups and SGLang on one machine still park ([Groups across machines](../guide/engines.md#groups-across-machines)).
