# CapyCTL 0.1.7

CapyCTL 0.1.7 stops work nobody is waiting for. When a client hangs up, on a streamed
or a non-streamed request, CapyCTL now stops the engine's work on it, and on SGLang it
aborts that one request and stops counting it as soon as the engine ends it. Each model
instance on vLLM 0.32.0 or SGLang 0.5.22 can get its own private LMCache, a KV cache in
host RAM, so a prompt that begins like an earlier one is answered faster, across a park
too. Engines no longer report usage off the host by default. vLLM 0.32.0 is supported for
chat, a discrete GPU's startup peak is measured instead of estimated, and failed SGLang
launches fail fast and clean up after themselves.

## Requests

- **A client that hangs up on a non-streamed request now stops the engine's work on
  it.** Until now only a streamed answer was stopped when its client went away; a
  non-streamed chat request or a decision request kept the engine busy until its whole
  answer was ready. CapyCTL now closes the engine connection as it does for streams,
  also for a model served on a host, and counts the request as it counts any cut
  request. See [When a request is cut short](../guide/requests.md#when-a-request-is-cut-short).
- **A streamed request whose client hangs up before its first token is cancelled at
  once.** It used to stay open on the engine, and uncounted as cancelling, until the
  engine produced its first token for it, which for a queued request could be long
  after.
- **On SGLang, a cancelled request stops counting as soon as the engine ends it.** On
  SGLang 0.5.22 with one tokenizer worker, CapyCTL sends each chat request its own
  request ID (the `X-CapyCTL-Request-Id` value, also the answer's `id`). When a client
  hangs up, or a request is cut by its deadline or idle bound, CapyCTL asks SGLang to
  abort that request and settles it when SGLang ends it, instead of waiting for the
  engine to read idle, on the router's machine and on hosts alike. When cancelled
  requests waiting for their instance's next stop fill a model's 32 places, CapyCTL
  restarts the instance, which settles them, rather than refusing new requests with
  `queue_full`.
- **A request cut before the engine answered it no longer settles on the engine's idle
  reading.** A request whose client hung up, or that missed its deadline, before the
  engine sent any of its answer may not have reached the engine's queue yet, so the
  engine reading idle proved nothing about it. Such a request now stays counted until
  the engine's next stop or restart; stopping and switching do not wait for it, and a
  model that would park stops instead. A request the engine had started answering, or
  answered with an error, settles on the idle reading as before.
  `capyctl status deployment` shows how many settle each way
  (`requests.settle_when_idle`, `requests.settle_at_stop`).
- How quickly each engine stops a request whose client hung up, measured on SGLang,
  vLLM, llama.cpp and TensorFold, is in the
  [requests guide](../guide/requests.md#when-a-request-is-cut-short).

## LMCache

- **A private shared KV cache per deployment.** On vLLM 0.32.0 and SGLang 0.5.22 or
  newer with LMCache 0.5.6 (or a later 0.5.x) installed and registered, each model
  instance gets its own LMCache server in host RAM, on loopback only, started and
  stopped with it, so a prompt that begins like an earlier one is answered from the
  cache after the engine's own cache dropped it, across a `restart_only` park too.
  `lmcache: auto` is the default, but a deployment that parks deep, the default
  residency, runs without LMCache until its engine, model type and GPU are qualified
  for deep parking with it, and this release qualifies none. Declare
  `residency: restart_only` to use LMCache now (see
  [Parking with LMCache](../guide/parking.md#parking-with-lmcache)).
  Set `lmcache: off` or `required` on a deployment, or `lmcache: off` on a host. The pool (4 GiB by default, `lmcache_pool`) and the LMCache
  server's own memory are reserved. `cache_salt` now reaches the engine and LMCache as a
  digest of the deployment, its revision and the salt. See
  [LMCache](../guide/engines.md#lmcache).

## Privacy

- **Engines no longer report usage off the host by default.** Every engine CapyCTL
  starts gets `DO_NOT_TRACK=1`, and vLLM also `VLLM_DO_NOT_TRACK=1` and
  `VLLM_NO_USAGE_STATS=1`, so vLLM no longer sends its usage report to
  `stats.vllm.ai`. A host that wants the engines' own defaults sets
  `engine_telemetry: allow`. A deployment or profile that sets one of these names is
  now refused (`engine_env_reserved:<name>`). See
  [Nothing reported off the host](../operations/data-retention.md#nothing-reported-off-the-host).
- **`retention: zero` never starts LMCache**, alongside keeping engine logs in memory
  and turning core dumps off. See
  [Zero data retention](../operations/data-retention.md#zero-data-retention).

## Engines

- **vLLM 0.32.0 is supported for chat**, deep parking included.

## Memory

- **On a discrete GPU the startup peak is measured.** A first start now measures the
  card and host RAM apart, from the engine's own processes, and later starts set aside
  what was measured instead of the estimate, on each domain once it is measured.
  `capyctl status deployment <name> --json` says where each figure came from
  (`startup.device_provenance`, `startup.host_provenance`). An enrolled host with a
  discrete GPU needs this release to start a deployment whose peak was measured that
  way (`host_capability_missing:startup_host_bytes` otherwise).

## Operations

- **Containers run on a discrete GPU.** On an RTX 4090 Laptop GPU with Docker and CDI,
  a standalone role running as an unprivileged user started, parked, woke and stopped
  vLLM 0.31.0 and started and stopped TensorFold 0.6.6; per-process GPU memory and the
  card's limits work as on a host. A TensorFold image needs `ninja`, and a new container
  started on existing state needs the same `--hostname` as the one before. See
  [Running in a container](../operations/install.md#running-in-a-container).

## Fixes

- **A changed hostname is reported clearly.** A standalone role started on existing
  state under another hostname, as a new container gets unless `docker run --hostname`
  is given, used to fail with an opaque `resource policy revision conflict`. It now
  refuses with `host_name_changed` (exit 2), naming the stored and the current name and
  the fix: `docker run --hostname <stored name>`.
- **Hosts running under a systemd user service or a template unit were refused.** A
  host whose memory is bounded by a cgroup named with `@` (`user@1000.service`,
  `capyctl@host.service`) reported it, and the server ended its session with
  `host session authorization failed`. Such hosts now join.
- **An SGLang launch whose scheduler dies while starting now fails within seconds.** It
  used to wait out the whole readiness deadline and then report that ownership of the
  engine was uncertain. CapyCTL now ends the engine's remaining processes, reports the
  launch as failed with the engine's own log lines, and releases its memory once they
  are gone. The repeated prometheus `FileNotFoundError` messages that followed such a
  failure no longer appear.
- **A failed SGLang launch no longer leaves shared memory behind.** A launch whose
  scheduler died while loading left SGLang's `/dev/shm/sglang_loads_*.shm` file, and
  other shared-memory segments its processes made, in RAM. CapyCTL now removes the
  segments the launch made, once its processes are verified gone and no other process
  holds them.
- **A failed SGLang launch's log ends with CapyCTL's failure line.** On Python 3.12,
  five lines about `ResourceTracker` and `ChildProcessError` used to follow it.

## Known limits

- On SGLang 0.5.22, a decision request whose client hung up still runs to its end in
  the engine
  ([sgl-project/sglang#45842](https://github.com/sgl-project/sglang/issues/45842));
  CapyCTL keeps it counted until the next stop.
- Decision deployments on SGLang run without LMCache for now: SGLang fails with it
  while its radix cache is disabled
  ([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)).
- Decision models (`api: decisions`) stay on SGLang 0.5.22 and llama.cpp 0.7.0. vLLM
  0.32.0 answers them from their text output instead of their decision head, so CapyCTL
  refuses vLLM for them with `capability_missing:decisions`
  ([Decision models](../guide/deploy.md#decision-models)).
