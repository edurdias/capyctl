# Status: ADR 0034 proposed: LMCache by default — 2026-10-24 (branch `docs/adr-lmcache-default`)

ADR 0034 (Proposed, for the owner to decide) plans "LMCache by default" for 0.2.0. In
0.1.8 the qualified table for LMCache beside a parking residency is empty, so on a GB10,
where every deployment parks deep, LMCache runs only for `restart_only` deployments. The
ADR reads what a deep park does to the engine and the connector: vLLM's sleep mode and
SGLang's memory saver allocate the KV cache through the CUDA virtual memory API, release
it on a park and make new memory at the same address on a wake, while LMCache's default
transfer has its server map the engine's KV through CUDA IPC handles, which LMCache's
own code says do not work on such memory; so today's rendering is expected to fail at the
engine's start with deep park, and a mapping that did work would hold the released KV and
go stale after the wake (an inference that live row LC12 checks). It proposes
engine-driven transfer for parking residencies (the server never maps the engine's KV),
names its costs (unmeasured speed; a pickle path any local process reaching the loopback
port could use to run code as the role's service user; no hybrid models in LMCache
0.5.6), and gives the qualification plan: live rows LC12–LC17 on host A with Qwen3-4B on
vLLM 0.32.0 and SGLang 0.5.22, pass criteria for correctness, memory, time and enrolled
hosts, and what happens when a combination fails (it stays out of the table and keeps deep
park with LMCache off, as in 0.1.8). It proposes `lmcache_pool: auto` as the default: the
pool holds one full context of the deployment's model after LMCache's eviction (KV bytes
per token from the context fit, times the context, divided by the 64 % that survives an
eviction), at least 4 GiB, capped at the host's `host_kv_limit`, with a warning when one
context does not fit, and an `auto` deployment never refused for its pool's room. It
proposes dropping the advice to declare `restart_only` for LMCache, since the benchmark
and the held-KV bound show a deep park without LMCache answering the first request after
a park sooner, and the reason `multi_gpu_unqualified` for ADR 0035's multi-GPU
deployments. Nine open questions for the owner carry options and a recommendation each.
No code, SPEC or guide text changes until the owner accepts it; CPU and Fake-engine tests
are not qualification, and LC12–LC17 are the live checks still needed.

# Release note: none
