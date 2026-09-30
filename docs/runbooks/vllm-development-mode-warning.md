# vLLM Development Mode — Security Warning (carried upstream)

> **This warning is carried verbatim from vLLM's security documentation and is NOT
> erased by capyctl's private binding, ingress gating, or policy controls.** It travels
> into every compatibility and release note for the deep-park path (F1 design §7,
> SPEC §9.1, upstream [S2]: docs.vllm.ai/en/latest/usage/security).

## What the upstream warning says

vLLM's security documentation warns against enabling development mode in production and
identifies the collective RPC surface as dangerous. capyctl's deep-park path
(`/sleep`, `/wake_up`, `/collective_rpc`) **requires development mode** — there is no
park without it (owner-confirmed dependency, F1 design §7).

## capyctl's controls (and what they do NOT change)

- The `vllm-sleep` profile **cannot launch in any mode** without the host-policy opt-in
  `security.allow_development_engine_controls: true` — the gate covers the profile, not
  just the operations (T21: default denial at fake/adapter/host-policy/controller layers).
- Engines bind private/loopback addresses from the host profile; the router's ingress
  gate validates generation and admission.
- **These controls do not make development mode production-safe.** The upstream warning
  stands: any co-tenant process that can reach the engine's control endpoints has access
  capyctl cannot fully mediate at F1.
- Production qualification requires a separately reviewed supported control path or
  appropriate upstream engine changes (release gate; not fillable by implementation).

## Where the gate lives

| Layer | Enforcement |
|---|---|
| Adapter | `VllmAdapter` refuses sleep/collective ops unless policy opted in |
| Controller | `vllm-sleep`-kind deployments cannot Start without the opt-in |
| Host policy | `security.allow_development_engine_controls` (default `false`) |
| Fake (F0 conformance) | `ParkPolicy::Denied` default; gate-mode conformance check |