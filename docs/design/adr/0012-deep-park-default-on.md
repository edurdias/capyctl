# ADR 0012 — Deep parking is on by default; a host opts out

**Status:** Accepted (owner decision 2026-09-17, reaffirmed 2026-09-22)
**Amends:** `SPEC.md` §9.1 (security gate), §16.2 (safe-example paragraph), §18 (F1
deliverable) and §20 T21; the deep-park bullet in `AGENTS.md` "Hard constraints".
**Unit:** W1 of `docs/plans/2026-09-22-two-host-control-plane-plan.md`
(matrix decision D1, gap G16).

## Context

On 2026-09-17 the owner ruled that parking is required product behavior and that deep
parking is on by default, with a host opting out ("It is opt out of deep parking. We
will do it by default"; `docs/specs/2026-09-17-native-launch-vllm-design.md`
§1 and §11). S1 shipped that default, and the S2 ADR was to amend SPEC §9.1, T21 and
`AGENTS.md` to match.

Commit `047007a` (2026-09-21) instead reversed the code default to explicit opt-in and
rewrote SPEC §9.1 and T21 to say that opt-in "supersedes the 2026-09-17 default-on
decision". No owner decision supports that text. Asked directly on 2026-09-22, the owner
reaffirmed default-on with a host opt-out.

On the hardware in hand (`host-a`, `host-b`) device and host memory are one
pool, so ADR 0010 refuses `host_backed` and `deep` is the only tier that frees memory.
For vLLM, deep parking runs through sleep mode, which needs vLLM's development mode.
vLLM's security documentation advises against development mode in production because it
also exposes the collective RPC surface [SPEC S2].

## Decision

1. **Default on.** A runtime profile that omits `security.deep_park` resolves with
   `deep_park: enabled`. `security.deep_park: disabled` is the host opt-out. A
   deployment declaring a parking residency (`deep`, `host_backed`) on an opted-out
   profile fails resolution with a message naming both settings; `restart_only`
   resolves. The opt-out is honored wherever resolution runs: the embedded coordinator
   and the remote host agent both resolve through `resolve_effective` against the host
   document they hold.
2. **Standalone.** `MLLM_DEEP_PARK` unset means on, `off` opts out, `on` is accepted
   explicitly. Any other value, an empty export included, is a configuration error
   (SPEC §15.3, T03), because a mistyped opt-out silently left on is the failure this
   switch exists to prevent.
3. **Provenance.** The effective configuration marks a defaulted value with
   `deep_park_source: default` (SPEC §7, T14). A declared value carries no marker, so
   every profile that states the switch serializes exactly as before. Provenance is
   derived, never declared: a host document naming `deep_park_source` is refused as an
   unknown field, and an effective snapshot that claims the default for a non-default
   value fails revalidation.
4. **Restart-only never sleeps.** SPEC §6.2 prohibits sleep calls for `restart_only`.
   The vLLM park policy is enabled only when deep parking is on *and* the residency
   parks, on both the embedded and the remote path (`mllm_adapters::vllm::park_policy`).
5. **The protections stay mandatory.** Default enablement relaxes none of them:
   - the engine listens on loopback only;
   - every launch has its own engine key that only mllm holds, never on argv;
   - every vLLM launch, embedded or remote, also has its own admin key
     (`MLLM_VLLM_ADMIN_KEY`, sealed under the binding's admin role like SGLang's).
     The guard admits the inference key only on `/v1`, `/v2`, `/inference`,
     `/cohere` and `/metrics`; every other route, the development controls
     included, takes the admin key alone. Ingress and the router hold only the
     inference key;
   - a development-mode vLLM launch loads mllm's key-guard middleware, and a host whose
     runtime directory lacks the guard module refuses the launch before any durable
     effect;
   - no engine control path (`/sleep`, `/wake_up`, `/collective_rpc`, SGLang's memory
     release) is reachable through host ingress or the router.

## Honest scope

This is not a production-safety claim. vLLM development mode remains an isolated
integration protected by the controls above, not a production-hardened control path;
production readiness still requires a separately reviewed supported control path or
engine changes (SPEC §9.1). CPU and Fake-engine tests of this default are not
qualification. SPEC §9.1 and T21 also require status to mark every profile that uses the
development controls; W1 adds effective-configuration provenance but does not add that
status surface, which remains open.

## Consequences

- Deep-park matrix rows (M28, M29, M34) need no per-host opt-in; M34 exercises the
  opt-out.
- A host profile that omitted `deep_park` under the opt-in interval now resolves with it
  enabled and a different recipe fingerprint. A frozen snapshot taken during that
  interval is a different recipe and is not silently reinterpreted.
- Separate admin key, migration (2026-09-24): an embedded vLLM launch recorded
  before the admin role sealed one key and runs the single-key guard. A restarted
  coordinator adopts it with that one key and never mints an admin key for it,
  because the running engine was not given one. It keeps the single-key guard
  until it next launches; every new launch seals both roles. The standalone host
  document now names `admin_credential_ref` for vLLM too, which changes the
  standalone vLLM recipe fingerprint (`admin_auth`).
- Standalone vLLM launches with sleep mode and development mode by default, the S1
  live-proven launch shape. Standalone SGLang's template declares `deep`, so
  `MLLM_DEEP_PARK=off` makes its deployment fail resolution rather than launch without
  its parking recipe.
