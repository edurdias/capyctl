# Status: Standalone host-KV limit is settable — 2026-10-23 (branch `fix/standalone-host-kv-limit`)

Found in the 0.1.7 LMCache measurement on host A (128 GB GB10): standalone fixed
`host_kv_limit` at a tenth of the observed memory (12.17 GiB), and
`--set host.resource_policy.memory.system.host_kv_limit=…` was refused as an unknown
standalone setting, so no LMCache pool above 12 GiB fitted (ADR 0033 §6 counts the pool
against it). With a 12 GiB pool only 17–28k of a 32k-token prompt came back after engine
eviction (TTFT 4.66 s); a 20 GiB pool on an enrolled host stating `host_kv_limit: 24GiB`
returned it all (0.35 s, against 7.85 s without LMCache).

ADR 0025 amendment of 2026-10-23: `host.resource_policy.memory.system.host_kv_limit` is a
stated standalone memory limit, by YAML, `--set` and
`CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__HOST_KV_LIMIT` (flag over environment
over YAML): `auto` (the tenth), a size or a whole percentage. Its bounds are an enrolled
host's `host_kv_limit`: zero or more, published as stated, never compared with the managed
limit (only the derived one is lowered to a lowered managed limit). On a discrete machine
it sets the `system` domain. The generated document states `host_kv_limit: auto`, so
`capyctl config show` lists it. A document without it, or with `auto`, publishes the same
policy, byte for byte. Documented in configuration ("Standalone host-KV limit") and the
engines guide's LMCache section (a long-prompt example with `lmcache_pool: 20GiB` and
`host_kv_limit=24GiB`).

Tests (failing before, passing after): the three channels and forms in the configuration
crate; the stated value replacing the derived one, kept above the managed limit and
normalizing as a host's, the discrete `system` domain, and the unchanged digest with
`auto`; a booted standalone storing each channel's value in the policy admission reads.
CPU and Fake-engine tests only; they are not qualification. Live check still needed: on
host A, a standalone started with `--set host.resource_policy.memory.system.host_kv_limit=24GiB`
admits a vLLM 0.32.0 deployment with `lmcache: required` and `lmcache_pool: 20GiB`, and
`capyctl status` shows the 20 GiB pool.

# Release note: Memory

- **Standalone's host-KV limit is settable.** What LMCache pools may hold together on
  standalone was fixed at a tenth of the memory (12.17 GiB on a 128 GB GB10), so no
  `lmcache_pool` above 12 GiB fitted. Raise it with
  `--set host.resource_policy.memory.system.host_kv_limit=24GiB`,
  `CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__HOST_KV_LIMIT=20%` or the document,
  as a size or a whole percentage, as a host's `host_kv_limit` takes it; `auto` keeps the
  tenth. A long prompt needs a larger pool to come back whole: see
  [Standalone host-KV limit](../operations/configuration.md#standalone-host-kv-limit) and
  [LMCache](../guide/engines.md#lmcache).
