# Status: ADR 0033 accepted: LMCache — 2026-10-21 (branch `docs/adr-0033-lmcache`)

ADR 0033 records the owner's decisions of 2026-10-21 for LMCache in 0.1.7, including the
owner's answers to the ten open questions of its first draft. LMCache 0.5.6 (the security
floor: CVE-2026-105192 is fixed in it), or a later 0.5.x release that passes CapyCTL's
launch-time check by shape, runs in multiprocess server mode only: vLLM 0.32.0 or newer
connects through LMCache's external `LMCacheMPConnector`, named with its module path, and
SGLang 0.5.22 or newer through `--enable-lmcache --lmcache-config-file`; the in-process
`LMCacheConnectorV1` is refused because it ignores `cache_salt` (LMCache issue #2878).
vLLM 0.31, older SGLang, llama.cpp, TensorFold and custom engines show "LMCache off" with
the reason. Every model gets LMCache by default, hybrid layouts and decision deployments
included (decisions on SGLang only: vLLM stays refused for decisions, ADR 0030 §6
amendment of 2026-10-21). Each deployment instance gets its own LMCache server, started and stopped by
CapyCTL with it, on loopback ports CapyCTL leases, checked with ADR 0031's listener check
because the 0.5.6 admin HTTP API has no authentication; the script API stays off; host
RAM (L1) only, LRU within the private pool, nothing on disk. The server stays up while
its model is parked, with its pool charged, and stops with the stop; a `restart_only`
deployment keeps a warm cache across the restart. Parking wins: a deep-parking deployment
uses LMCache only on an engine version, model type and GPU combination qualified live,
and elsewhere keeps deep parking and runs with LMCache off and the reason shown. An
engine that fails to start with the connector is relaunched once without LMCache, with a
warning and an `lmcache_off` event (under `lmcache: required` the launch fails); a server
that dies while its engine runs fails the instance, which recovers with both relaunched.
`cache_salt` is no longer forwarded unchanged: vLLM, SGLang and LMCache receive a
domain-separated SHA-256 digest (64 hex characters) of the deployment, its revision and
the caller's salt; no salt stays no salt; a salt is not authorization. The pool is 4 GiB
per instance by default (`lmcache_pool`), charged with the server's measured overhead in
host RAM; the overhead allowance and the server's start bound are set from live row LC1's
measurements plus a stated margin, and no number is committed before that. LMCache is on
by default (`lmcache: auto`); a deployment's `lmcache: off` opts out, `lmcache: required`
refuses to start rather than run without it, and a host's `lmcache: off` turns it off for
every deployment on the machine and refuses `required` there. Zero data retention: a host
or deployment `retention: zero` keeps engine logs in memory, starts engine and LMCache
processes with no core dumps, and never starts LMCache; turning it on for a host stops
existing LMCache servers; the engine's own GPU prefix caches are not covered. The ADR
also gives the engine-option changes (SGLang's LMCache options owned, an LMCache
`--kv-transfer-config` refused from vLLM `extra_args`, `LMCACHE_*` reserved), the metrics
read from the server's `/metrics`, the refusal codes, conformance row T45, and live rows
LC1–LC11 (decisions, the relaunch without LMCache, a server exit, a host's `off` and
`required` refused on it among them). `AGENTS.md` lists the two host A qualification
environments the owner approved on 2026-10-21, `~/vllm-0.32.0-lmcache-venv` and
`~/sglang-0.5.22-lmcache-venv`. Nothing is implemented; the SPEC, guide and
data-retention changes the ADR lists land with the implementation. No code or tests
change; CPU and Fake-engine tests are not qualification, and the LC rows are the live
check still needed.

# Release note: none
