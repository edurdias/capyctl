# Status: ADR 0033 accepted: LMCache — 2026-10-21 (branch `docs/adr-0033-lmcache`)

ADR 0033 records the owner's decisions of 2026-10-21 for LMCache in 0.1.7. LMCache 0.5.6
(the security floor: CVE-2026-105192 is fixed in it) runs in multiprocess server mode
only: vLLM 0.32.0 or newer connects through LMCache's external `LMCacheMPConnector`, named
with its module path, and SGLang 0.5.22 through `--enable-lmcache --lmcache-config-file`;
the in-process `LMCacheConnectorV1` is refused because it ignores `cache_salt` (LMCache
issue #2878). vLLM 0.31, other SGLang versions, llama.cpp, TensorFold and custom engines
show "LMCache off" with the reason. Each deployment instance gets its own LMCache server,
started and stopped by CapyCTL with it, on loopback ports CapyCTL leases, checked with ADR
0031's listener check because the 0.5.6 admin HTTP API has no authentication; the script
API stays off; host RAM (L1) only, LRU within the private pool, nothing on disk. The
server stays up while its model is parked, with its pool charged, and stops with the
stop. Deep parking with LMCache is allowed only for engine version, model type and GPU
combinations qualified live on host A; other combinations run `restart_only` and keep the
server, and its cache, across the restart (LMCache issue #4071). `cache_salt` is no
longer forwarded unchanged: vLLM, SGLang and LMCache receive a domain-separated SHA-256
digest (64 hex characters) of the deployment, its revision and the caller's salt; no salt
stays no salt; a salt is not authorization. The pool is 4 GiB per instance by default
(`lmcache_pool`), charged with the server's measured overhead in host RAM. LMCache is on
by default (`lmcache: auto`); `lmcache: off` opts out, and `lmcache: required` refuses to
start rather than run without it. Zero data retention: a host or deployment `retention:
zero` keeps engine logs in memory and never starts LMCache; turning it on for a host stops
existing LMCache servers; the engine's own GPU prefix caches are not covered. The ADR
also gives the engine-option changes (SGLang's LMCache options owned, an LMCache
`--kv-transfer-config` refused from vLLM `extra_args`, `LMCACHE_*` reserved), the
metrics read from the server's `/metrics`, the refusal codes, conformance row T45, live
rows LC1–LC6, and ten open questions on details the owner did not decide. `AGENTS.md`
lists the two host A qualification environments the owner approved on 2026-10-21,
`~/vllm-0.32.0-lmcache-venv` and `~/sglang-0.5.22-lmcache-venv`. Nothing is implemented;
the SPEC, guide and data-retention changes the ADR lists land with the implementation.
No code or tests change; CPU and Fake-engine tests are not qualification, and the LC rows
are the live check still needed.

# Release note: none
