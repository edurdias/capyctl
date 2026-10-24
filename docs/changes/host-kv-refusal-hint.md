# Status: A start refused for host-KV room says which limit to raise — 2026-10-23 (branch `fix/host-kv-refusal-hint`)

Found in the live check of the settable standalone host-KV limit on host A (GB10): a vLLM
0.32.0 deployment with `lmcache: required` and `lmcache_pool: 20GiB` on a standalone with
the derived 12.17 GiB host-KV limit was refused, exit 4 `insufficient_resources`, with only
`capacity_blocked: Capacity is unavailable: host spark-0292: insufficient_capacity; wait,
stop another deployment, or start with --evict`, so nothing told the operator that the
host-KV limit was what to raise.

ADR 0033 §6 counts a deployment's host-KV need (its LMCache pool, and any `host_kv_bytes`
its resources declare) against the host's `host_kv_limit`; ADR 0025's amendment of
2026-10-23 made standalone's settable. When no managed limit is short but a host-KV limit
is, the placement refusal now names it, per host:
`host <h>: insufficient_capacity: host_kv (needs 20.0 GiB, limit 12.2 GiB; raise
host.resource_policy.memory.system.host_kv_limit or lower lmcache_pool)`, with `, <n> GiB
already held` when other owners hold host-KV room there. Standalone's embedded host (the
only host not enrolled) names the standalone setting; an enrolled host names its own
`resource_policy.domains.<domain>.host_kv_limit`. What to lower is `lmcache_pool` when the
need includes an LMCache pool, the deployment's `host_kv_bytes` when it declares them, or
both. The same words reach every place the refusal is shown: the management refusal
(`capacity_blocked`, its message), so the CLI's text and JSON (`code` and `message`,
unchanged fields); an instance deferred for it records
`placement: insufficient_capacity: host_kv (…)` as its `last_error` (status, `start
--wait`), and the `instance_start_deferred` event carries the same reason; a start with
`--evict` that eviction cannot fit names it when releasing would leave the memory but not
the host-KV room. The closed code (`capacity_blocked`, `insufficient_resources`, exit 4)
is unchanged, and a refusal by any other limit (a managed limit short, even beside a
host-KV one, parked limits, device conflicts, gates) reads as before. The admission check
at arm is unchanged. Errors guide (the `insufficient_resources` row and its LMCache
section) and configuration ("Standalone host-KV limit") updated.

Tests (failing before, passing after): scheduler — a host-KV refusal names host_kv, the
need, what is held and the limit, standalone's or an enrolled host's setting, on one host
and on two; a managed limit short as well is named as before and the `last_error` reason is
the code alone. Store — a standalone start with a 12 GiB pool against an 8 GiB limit is
`CapacityBlocked` naming host_kv and standalone's setting, and a second instance deferred
for it records the same words in `last_error`; a start refused by the host-KV limit of two
enrolled hosts names each host's `resource_policy.domains.unified.host_kv_limit`, and so
does the plan of the same start with eviction. CPU and
Fake-engine tests only; they are not qualification. Live check still needed: on host A, a
standalone without `host_kv_limit` refuses the 20 GiB pool deployment with the message
above.

# Release note: Memory

- **A start refused for lack of host-KV room says which limit to raise.** When a model's
  LMCache pool does not fit the machine's host-KV limit, the refusal (still
  `insufficient_resources`, exit 4) now says `host_kv` with what the model needs, what
  other models already hold, the limit and the setting to raise, for example
  `host_kv (needs 20.0 GiB, limit 12.2 GiB; raise host.resource_policy.memory.system.host_kv_limit or lower lmcache_pool)`;
  on a machine of several it names that machine's
  `resource_policy.domains.<domain>.host_kv_limit`. See
  [errors](../guide/errors.md#lmcache).
