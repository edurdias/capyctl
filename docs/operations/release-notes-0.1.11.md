# CapyCTL 0.1.11

CapyCTL 0.1.11 fixes problems found on 0.1.10. On a discrete GPU, a model
that states its memory request starts on a card it fits, can share a large
card with another model, and a start that needs more memory than a whole
limit names the setting to raise. Streams that fail before their first token
end with an error event, answers come back exactly as the engine wrote them,
and an editable SGLang is refused when you add it. It changes no protocol: a
0.1.11 server and 0.1.10 hosts work together, and the other way round.

## Fixes

- **A model on a discrete GPU with a stated memory request starts when its
  running charge fits the card.** Its first start was charged 2.5 GiB above
  the request (graph allowance and CUDA context) when the deployment was
  accepted before its checkpoint was measured, so on a 16 GB card a model
  whose running charge fit could be refused at every start. The first start
  is now charged the request plus the CUDA context until measured. A
  deployment whose start cannot fit the card it was resolved for, such as
  one whose `memory.startup` is too large, is named by `capyctl validate
  config --host` and `capyctl status deployment`, with the host, the card,
  the charge and its limit. A deployment accepted by an earlier release keeps
  its figure; deploy it with `memory.startup` equal to its request. Hosts
  from 0.1.7 on take the server's figure for such a start; upgrade older
  hosts with the server.
- **A vLLM model on a discrete GPU can share the card when you state its
  memory request.** CapyCTL sizes a vLLM request on a discrete GPU at 75 % of
  the card or more, a guard for a start failure seen with vLLM 0.29, and that
  stays for deployments that state only their KV cache. A deployment that
  states `engine_config.memory.request` was charged that request, but vLLM was
  still told to check for 75 % of the card free at start, so it could not
  start beside another model. vLLM is now told the stated request's share of
  the card. To run two models on one large card, state the vLLM model's
  request from its measured peak (`startup.measured` in `capyctl status
  deployment <name> --json`, less 1.25 GiB for the CUDA context). On
  upgrade, a vLLM deployment already accepted with a stated request below
  75 % of its card is told that smaller share at its next start; if the
  model needs more it fails to start (`launch_failed`), and raising
  `engine_config.memory.request` fixes it.
- **A start that needs more memory than a whole limit says which setting to
  raise.** A deployment whose measured startup peak was above a managed limit
  (host RAM on a discrete GPU, where vLLM's loader can hold well beyond the
  weights) could start once and then never again, refused with advice to wait,
  stop another deployment or use `--evict`, none of which helps. The refusal
  now says the need is above the whole limit and names the setting, for
  example `host.resource_policy.memory.system.managed_limit` on standalone
  (issue #241).
- **A streaming request that fails before its first token now ends with an
  error event** instead of an empty HTTP 200 stream. When a serving host's
  ingress is at its forward limit, the request goes to another instance of
  the model, or the client gets a retryable `unavailable` error.
- **Answers come back exactly as the engine wrote them.** The router used to
  re-encode every answer, which could move a decision's probabilities or a
  chat answer's log probabilities by one unit in the last place, respell
  numbers and sort keys. Each streamed chunk is now the engine's bytes with
  only `model` set to the name you sent (a stream's figures still arrive as
  the `: x-capyctl-metrics` comment line). A `/v1/systemone` answer is the
  engine's bytes with `model` set and `capyctl` added, and a collected chat
  answer, which also gains `capyctl`, carries the engine's log
  probabilities, `usage` and statistics as the engine sent them (issue #223).
- **An editable SGLang is refused when you add it, with a clear code.**
  SGLang installed with `pip install -e`, as in the official
  `lmsysorg/sglang` image, cannot be loaded by CapyCTL, which never runs the
  `.pth` file such an install depends on. `capyctl engine add` now refuses it
  (`installation_unsupported:editable`, exit 17), and standalone refuses to
  start one with the same code, instead of each launch failing a second in
  with `startup_error`. On a host enrolled with a server the launch still
  starts and now fails with `engine_import_failed` and a hint of what to
  check; the refusal on such hosts follows in 0.2.0. Reinstall SGLang without
  `-e`.
- **Engines start more reliably beside others on unified memory.** On a
  unified-memory machine such as a GB10, CapyCTL now drops the page cache of a
  model's checkpoint files right before its engine starts, without
  privileges and without touching the rest of the cache. An engine started
  beside a running one could fail creating its CUDA context with
  `CUDA error: out of memory` while most of the memory was page cache, some
  of it the model's own checkpoint. If the drop fails,
  the host log says so once and the engine starts anyway. Discrete GPUs are
  unchanged. See [installation](../operations/install.md#discrete-nvidia-gpus).
- **The engine log of a launch that failed is readable.** The management
  engine-log read answered `404` for a launch whose engine exited before it was
  ready, so the one record of why it failed could not be read through the API.
  It now returns the end of the instance's latest launch log, including a
  failed one (with `engine_logs: memory`, for 60 seconds after the engine
  exited). See [engine logs](../operations/install.md#engine-logs-and-troubleshooting).
