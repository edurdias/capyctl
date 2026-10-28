# Status: ADR 0034 accepted: LMCache by default — 2026-10-27 (branch `docs/adr-lmcache-default`)

ADR 0034 (Accepted 2026-10-27, target 0.2.0, retargeted from 0.1.9) plans "LMCache by
default". The owner answered its nine open questions on 2026-10-27; the answers are
recorded in the ADR ("Owner decisions of 2026-10-27"). In 0.1.8 the qualified table for
LMCache beside a parking residency is empty, so on a GB10, where every deployment parks
deep, LMCache runs only for `restart_only` deployments. The ADR reads what a deep park
does to the engine and the connector: vLLM's sleep mode and SGLang's memory saver allocate
the KV cache through the CUDA virtual memory API, release it on a park and make new memory
at the same address on a wake, while LMCache's default transfer has its server map the
engine's KV through CUDA IPC handles, which LMCache's own code says do not work on such
memory (an inference that live row LC12 checks). Parking residencies therefore use
engine-driven transfer (the server never maps the engine's KV), with its pickle path on by
default and documented beside ADR 0033's "Local reach", the host's `lmcache: off` as the
remedy (owner decision 1). `lmcache_pool: auto` becomes the default: one full context of
the deployment's model after LMCache's eviction, at least 4 GiB, capped at the host's
`host_kv_limit`, with a warning when one context does not fit; an `auto` pool that does
not fit the room left shrinks to it, or runs without LMCache (`host_kv_room`), and is
never refused (owner decisions 2 and 3). Standalone's `host_kv_limit` stays at 10 % of the
memory it observes at every start, and settable (owner decision 4). Every LMCache launch,
`restart_only` included, moves to engine-driven transfer once LC16's cost bounds and a new
restart-park row, LC18, pass (owner decision 5); the advice to declare `restart_only` for
LMCache is dropped (owner decision 6); only dense full-attention model types qualify in
0.2.0 (owner decision 8).

Decisions that changed the design: `host_backed` on a discrete GPU is qualified in 0.2.0,
with a vLLM 0.32.0 or SGLang 0.5.22 environment on a maintainer's discrete-GPU machine (the
RTX 4090 laptop) or a VM rented from Shadeform, where LC13–LC16 run with `host_backed`
(owner decision 7); the model types beyond `qwen3` are the most-used open-weight dense chat
model types on OpenRouter that fit the hosts, a list compiled when the implementation is
built and shown to the owner before the live rows (owner decision 9); and, under the
owner's rule of 2026-10-27 on certified combinations, the qualified table is the certified
set: `lmcache: required` outside it, or on a model across several GPUs, runs LMCache as an
uncertified setup instead of being refused `lmcache_unavailable:park_unqualified` or
`:multi_gpu_unqualified`, while `lmcache: auto` keeps LMCache off there. No code, SPEC or
guide text changes yet; CPU and Fake-engine tests are not qualification, and LC12–LC18 on
host A and LC13–LC16 on the discrete machine are the live checks still needed.

# Release note: none
