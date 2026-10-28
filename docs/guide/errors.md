# Exit codes and errors

Every CapyCTL command exits 0 on success. On failure it prints one line to
standard error, `error [<code>]: <message>`, and exits with the status below.
With `--format json` the error is a JSON object with `code` and `message`.

| Exit | Code | What it means | What to do |
|---|---|---|---|
| 2 | `invalid_config` | A document or command argument is invalid. | Run `capyctl validate config --file <file>` and fix what it reports. |
| 2 | `not_found` | The deployment, instance or host named does not exist. | Check the name with `capyctl list deployments` or `capyctl list hosts`. |
| 2 | `device_policy_mismatch` | A host's file describes a card that does not match the GPU it found. | Fix the `gpuN` entries of `resource_policy` to match `nvidia-smi -L`. |
| 2 | `missing_system_allocation` | A deployment that lists its own `resources` on a discrete card leaves out host RAM (`system`). | Add the `system` entry, or leave `resources` out and let CapyCTL size it. |
| 2 | `host_name_changed` | A standalone role's state belongs to the host it was first started under, named after the machine's hostname, and the hostname has changed since; a container gets a new hostname each time it is created. The message names the stored name and the current one. Nothing is changed. | Start it under the stored hostname: for a container, `docker run --hostname <stored name>` ([Running in a container](../operations/install.md#running-in-a-container)); on a machine, set the hostname back. |
| 2 | `group_placement_required` | A deployment with a `topology` and two or more `placement.hosts` also sets `host`, `selector`, `strategy` or `max_per_host`. | List the machines in `placement.hosts`, head first, and remove the other placement fields ([One model across machines](several-machines.md#one-model-across-machines)). A `topology` without `placement.hosts` runs on several GPUs of one machine instead ([One model on several GPUs](deploy.md#one-model-on-several-gpus)). |
| 2 | `group_topology_invalid` | `placement.hosts` repeats a machine, a dimension of `topology` is zero, the number of machines does not divide `tensor_parallel` × `pipeline_parallel`, or `topology: auto` names two or more machines (a group's shape is yours to state). | Fix `topology` or `placement.hosts`. |
| 2 | `topology_devices_mismatch` | `devices` names a different number of GPUs than `topology.tensor_parallel`, or any GPU beside `topology: auto`. | Name exactly as many GPUs as `tensor_parallel`, or leave `devices` out and let CapyCTL pick them. |
| 2 | `group_profile_mismatch` | The engine profile does not exist on one of the machines, or its build differs between them. | Register the same engine build under the same name on every machine. |
| 2 | `group_checkpoint_mismatch` | The model's files differ between the machines; the message lists each machine's digest. | Make the copies identical, or let CapyCTL download the model on each machine. |
| 2 | `group_model_path_mismatch` | A SGLang group with `residency: deep` has the model at different paths on its machines; the message names both. | Put the model at the same path on every machine, or use `residency: restart_only`. |
| 2 | `peer_address_missing` | A machine named in `placement.hosts` declares no peer address. | Set `resource_policy.groups.peer_address` (or `--peer-address`) on that machine and restart CapyCTL there. |
| 2 | `peer_address_not_local` | The declared peer address is not on any network interface of that machine. | Declare an address the machine holds on its direct link. |
| 2 | `engine_env_reserved:<name>` | An engine environment variable CapyCTL sets itself (for example `NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`, `CUDA_VISIBLE_DEVICES`), any `LMCACHE_*` variable, which LMCache would read as configuration, or an engine tracing variable: `OTEL_*`, `SGLANG_TRACE_*`, `SGLANG_OTLP_*`, `TRACEPARENT` or `TRACESTATE`, in any case. | Remove it; CapyCTL renders these names ([the full list](../operations/configuration.md#engine-environment)). For engine tracing set `observability.tracing.engines: on` on the host ([Engine tracing](engines.md#engine-tracing)). |
| 2 | `engine_env_not_approved:<name>` | A deployment sets an engine variable the engine profile does not approve (on some machine of a group). | Approve it with `capyctl engine add ... --approve-env <name>` on every machine, or remove it ([Engine variables](engines.md#engine-variables)). |
| 2 | `engine_env_conflict:<name>` | The same variable is in the file and on the command line. | Give it one way only. |
| 2 | `custom_field_unsupported:<field>` | A deployment on a custom engine states something the engine's manifest owns: `extra_args`, `accept_extra_args`, a tool-call or reasoning parser, `memory`, a draft model or a typed engine field such as `dtype` or `max_concurrent_requests`. | Remove it; a custom engine's deployment states only `engine_config.context_length` and `engine_config.env` ([Custom engines](engines.md#custom-engines)). |
| 2 | `custom_placeholder_unset:context_length` | The custom engine's manifest uses `{context_length}` and the deployment states no `engine_config.context_length`. | Add `engine_config.context_length` to the deployment. |
| 2 | `custom_value_invalid:<placeholder>` | A deployment on a custom engine gives `{served_name}` (its first route) or `{model_dir}` (its model path) a value that starts with `-` or holds a control character, so the engine could read it as an option. | Rename the route or move the model; a deployment cannot add options to a custom engine's command. |
| 2 | `custom_credential_ref_missing` | A custom engine profile written by hand states no `security.credential_ref`, or a blank one. | Add `credential_ref: "secret://engine-key"` under the profile's `security` ([Custom engines](../operations/configuration.md#custom-engines)). |
| 2 | `declined` | `capyctl engine add --custom` asked for confirmation and the answer was not `y` or `yes`. Nothing was written. | Run it again and answer `y`, after checking what it prints. |
| 3 | `unauthorized` | The credentials were refused. | Check the admin token or the host's identity. |
| 4 | `insufficient_resources` | No host has the GPU memory the deployment needs. The message names each host's limit, for example `host a needs 64.0 GiB of gpu0 memory, 60.8 GiB free of its 60.8 GiB limit`. When the memory is there but the room for KV cache kept in host RAM (an LMCache pool) is not, it says `host_kv` with the numbers and the setting to raise, for example `host spark: insufficient_capacity: host_kv (needs 20.0 GiB, limit 12.2 GiB; raise host.resource_policy.memory.system.host_kv_limit or lower lmcache_pool)`. | Stop or park another deployment, start with `--evict`, or lower the deployment's memory. For `host_kv`, raise the setting it names ([Standalone host-KV limit](../operations/configuration.md#standalone-host-kv-limit); on a machine of several, its document's `resource_policy.domains.<domain>.host_kv_limit`) or lower `lmcache_pool`. |
| 4 | `insufficient_device_memory` | The card cannot hold the model, or another program holds its memory now. On a machine with several GPUs the message names the `tensor_parallel` whose share would fit each card and how many GPUs that needs, or says that no number of the machine's GPUs fits and gives the share per GPU at that number. | Use a smaller or quantized checkpoint, a smaller KV cache, free the card, or spread the model with the `topology` the message names ([One model on several GPUs](deploy.md#one-model-on-several-gpus)). vLLM needs a card of about 10 GiB or more. |
| 4 | `insufficient_gpus` | No allowed machine has as many identical GPUs as `tensor_parallel`; the message names the number needed, each machine's count, and the `tensor_parallel` that would fit. | Lower `tensor_parallel`, use `topology: auto`, or place the model on a machine with more GPUs. |
| 4 | `device_unobserved` | `nvidia-smi` gave no fresh reading for that GPU, so nothing new starts on it. | Check that `nvidia-smi` works on that machine, then retry. |
| 4 | `rendezvous_ports_exhausted` | Every rendezvous port of the head machine's range is held by another group. | Stop a group on that machine, or widen `--rendezvous-ports`. |
| 4 | `rendezvous_port_in_use:<port>` | Another program holds the rendezvous port on the head machine. CapyCTL picks another port on the next start. | Retry; if it persists, find the program holding the port. |
| 4 | `service_port_in_use:<port>` | Another program holds the head's engine port, or a SGLang worker's local port. | Retry; if it persists, find the program holding the port. |
| 4 | `host_tuning_missing:<item>` | With `require_rdma: true`, a machine lacks `memlock` (unlimited locked memory) or `infiniband` (read-write `/dev/infiniband/uverbs*`). | Fix the item as root on that machine, or set `require_rdma: false`. |
| 5 | `unsupported` | The request is valid but this release cannot do it. | Read the message; it names the missing capability. |
| 5 | `store_from_newer_version` | The state directory was written by a newer CapyCTL. | Run the newer binary, or restore a backup taken before the upgrade. |
| 5 | `multi_gpu_unsupported` | The deployment names two or more GPUs without a `topology`, or asks for several GPUs on a machine with one unified-memory GPU (such as a GB10) or on an engine that runs on one GPU only (TensorFold, a custom engine). | Add `topology: {tensor_parallel: N}` with N the number of GPUs you named ([One model on several GPUs](deploy.md#one-model-on-several-gpus)); otherwise use one GPU, or several machines with `placement.hosts`. |
| 5 | `multi_gpu_mixed_devices` | The GPUs a deployment names differ in product or memory. | Name identical GPUs (`nvidia-smi -L` shows them). |
| 5 | `tensor_parallel_indivisible` | On vLLM or SGLang, `tensor_parallel` does not divide the model's attention heads; the message names the head count and the numbers that divide it. | Use one of the numbers the message names, or `topology: auto`. |
| 5 | `unsupported_gpu_topology` | The machine has both an integrated and a discrete GPU. | Not supported in this release. |
| 5 | `host_backed_unavailable` | `residency: host_backed` on unified memory. | Use `deep`, or leave `residency` out. |
| 5 | `capability_missing`, `capability_missing:deep_park` | `--deep-park enabled` on an engine that cannot park (TensorFold, llama.cpp, a custom engine), or a deployment asking such an engine to park (`residency: deep` or `host_backed`). | Leave `--deep-park` out; use `residency: restart_only`. |
| 5 | `group_shape_unsupported`, `group_shape_unsupported:<engine>` | More than one rank per machine in a group, `pipeline_parallel` above 1 on one machine, or a shape the engine cannot run (TensorFold runs `tensor_parallel: 2` on exactly two machines; a custom engine, `group_shape_unsupported:custom`, runs on one machine only). | Use one machine per rank and a shape the engine supports ([Groups across machines](engines.md#groups-across-machines)); on one machine use `tensor_parallel` only; for a custom engine, remove `topology` and use `instances` for more copies. |
| 5 | `group_instances_unsupported` | `instances` above 1 with a `topology` across machines. | Deploy one group per deployment. |
| 5 | `group_drift:<field>`, `tp_drift:<field>` | The engine changed a multi-machine or multi-GPU setting CapyCTL rendered (the field is named). | Report it; the engine build behaves differently from the one CapyCTL supports. |
| 5 | `host_capability_missing:engine_groups` | A machine runs a CapyCTL too old for groups. | Upgrade CapyCTL on that machine. |
| 5 | `host_capability_missing:local_tensor_parallel` | A deployment with a `topology` on one machine names a machine that runs a CapyCTL too old to run a model on several GPUs; nothing was sent to it. | Upgrade CapyCTL on that machine. |
| 5 | `capability_missing:decisions` | A deployment with `api: decisions` names an engine that cannot serve decision models: vLLM (any version: 0.32.0 answers Clef and Clef-Flash from their text output instead of their decision head), TensorFold, llama.cpp before 0.7.0, SGLang before 0.5.22, an installation whose registered build names no version, or a custom engine whose manifest says `api: chat`. | Use llama.cpp 0.7.0 or newer or SGLang 0.5.22 or newer, registered with `capyctl engine add`, or a custom engine with `api: decisions` ([Decision models](deploy.md#decision-models)). |
| 5 | `capability_missing:chat` | A chat deployment (no `api`, or `api: chat`) names a custom engine whose manifest says `api: decisions`. | Add `api: decisions` to the deployment, or use a chat engine. |
| 5 | `host_capability_missing:decisions` | A machine runs a CapyCTL too old for decision models. | Upgrade CapyCTL on that machine. |
| 5 | `host_capability_missing:custom_engines` | The deployment's engine is a custom engine and a machine it names runs a CapyCTL too old for custom engines. | Upgrade CapyCTL on that machine. |
| 5 | `lmcache_unavailable:<reason>` | A deployment with `lmcache: required` cannot use LMCache, for the reason named after the colon ([LMCache](#lmcache) lists them). A reason known before anything starts (the machine's `lmcache: off`, the engine or its version, a missing or unsupported LMCache package, a `topology`, a parking residency not qualified, the deployment's own `--kv-transfer-config`) refuses it at deploy or start; one found when the launch runs (the launch-time check, the server's start, its listener check, the engine's start with LMCache) fails the launch, and nothing is left running. | Remove the reason ([LMCache](#lmcache) says how for each), or set `lmcache: auto`, which runs without LMCache where it cannot be used. |
| 5 | `retention_zero:lmcache` | `lmcache: required` beside `retention: zero`, the deployment's own or that of a machine it names. Zero retention never runs LMCache. | Drop `lmcache: required` or the deployment's `retention: zero`, or place it on a machine without `retention: zero`. |
| 5 | `host_capability_missing:lmcache` | A deployment with `lmcache: required` names a machine that runs a CapyCTL too old for LMCache. | Upgrade CapyCTL on that machine, or set `lmcache: auto` (it then runs there without LMCache). |
| 5 | `host_capability_missing:retention` | A deployment with `retention: zero` names a machine that runs a CapyCTL too old to honour it. | Upgrade CapyCTL on that machine. |
| 6 | `unreconciled` | CapyCTL cannot yet tell what an engine is doing, so it will not act on it. | Wait for the host to reconnect, then retry. |
| 7 | `device_conflict` | Another deployment holds the GPU exclusively. | Stop that deployment or choose another device. |
| 8 | `category_limit` | A host limit on how many models of this kind may run was reached. | Stop one, or raise the limit in the host document. |
| 10 | `activation_timeout` | The engine did not become ready in time. | Check the engine log; for large models raise the deployment's `timeouts.initialize`, or pass `--initialize-timeout` to `capyctl start deployment`. |
| 11 | `topology_unknown` | The host has not reported its GPUs yet. | Wait for the host to finish starting, then retry. |
| 12 | `no_safe_estimate` | CapyCTL cannot estimate the model's memory safely. | Declare the deployment's resources explicitly. |
| 13 | `internal` | An internal, storage or I/O failure, or a launch that failed (`operation_failed`). | Read the message, `capyctl status deployment <name>` and the role's log. |
| 14 | `host_revoked` | The server revoked this host. The host exits and is not restarted. | Bring it back (see [Several machines](several-machines.md#take-a-machine-out)). |
| 15 | `host_ineligible` | No allowed host can take the deployment: drained, revoked, offline or too old. | Undrain, reconnect, upgrade or re-enroll the host the message names. |
| 16 | `engine_not_found` | The path holds no `vllm`, `sglang` or `tensorfold` package and is no `llama-server` binary; or, with `--custom`, the executable is missing or is not an executable file. | Name the environment, its `bin/vllm`, `bin/tensorfold` or `bin/python3`, or a `llama-server` binary or its directory, or search with `capyctl engine detect --path <dir>`; with `--custom`, name the program itself. |
| 17 | `engine_unsupported` | The package is not a supported engine; or `llama-server --version` printed no version line CapyCTL reads; or the machine has `/etc/llama.cpp/config.ini`, which llama-server would read as options. | Register a vLLM, SGLang, TensorFold or llama.cpp installation; for llama.cpp, remove `/etc/llama.cpp/config.ini`. |
| 18 | `engine_version_failed` | The engine's version check failed or timed out; nothing was written. | Repair the installation until its version check succeeds, then add it again. |
| 19 | `profile_exists` | The engine profile name is taken. | Pass `--name`, or remove the existing profile first. |
| 20 | `profile_in_use` | Removing or replacing the engine would affect the deployments listed. | Stop them, or rerun with `--drain`. |
| 21 | `publish_rejected` | The server refused the updated engine list; the reason follows. | Fix what the reason names. |
| 22 | `agent_unreachable` | CapyCTL on this machine is running but did not answer an engine command. | Retry; if the message says the outcome is unknown, run `capyctl engine list` first. |
| 23 | `not_interactive` | `capyctl engine add` without a path needs a terminal to pick one; `capyctl engine add --custom` needs a terminal to confirm. | Name the installation; with `--custom`, pass `--yes` once you have checked the manifest. |
| 24 | `profile_not_published` | No allowed host offers the engine profile the deployment names; nothing was stored. | Register the engine on a host with `capyctl engine add <path> --name <profile>`, then deploy again. |
| 25 | `still_stopping` | A `start` came right after a `stop`, before the engine finished going away, or while CapyCTL was still confirming that a slow stop's engine exited; nothing was started. | Retry in a moment, or run `capyctl start deployment <name> --wait`, which waits for the stop and then starts. |
| 26 | `toolchain_missing` | `capyctl engine add` found a TensorFold installation, but `ninja`, `nvcc` or a C++ compiler is not on the engine's PATH (its `bin`, the CUDA toolkit's `bin`, then `/usr/local/bin`, `/usr/bin`, `/bin`). TensorFold builds CUDA kernels on its first start. Nothing was written. | Install what the message names into one of those directories, or set `CUDA_HOME` to a toolkit that has `nvcc`, then add the engine again. |
| 27 | `custom_manifest_invalid` | `capyctl engine add --custom` could not use the manifest: it is unreadable, larger than 64 KiB, has an unknown field, uses an unknown placeholder or leaves out `{host}`, `{port}` or `{model_dir}`, puts a placeholder in `health` or `idle` or `{api_key}` outside `args` and `env`, has a `health` or `idle.url` that is not a path, or an `environment` that is not an absolute directory. Also a hand-written custom engine profile whose `manifest_digest` does not match. Nothing was written. | Fix what the message names ([The manifest](engines.md#the-manifest)), then add the engine again. |

The systemd units restart CapyCTL when it fails, except on exits a restart cannot
fix: 2, 3 and 5, and 14 for a host.

A group running across machines can also show these codes in
`capyctl status deployment <name>` (the instance's `LAST ERROR` and the
member's row). They describe what happened to a running group; no command
exits with them:

- `group_member_failed`: a member exited or failed; its row shows `failed`.
  CapyCTL stops the whole group.
- `group_member_uncertain`: a member's machine cannot be reached. The member
  keeps its memory charged until that machine proves it gone.
- `group_wake_mismatch`: after a wake, the model answered a fixed prompt
  differently than before the park. CapyCTL stops the group.
- `group_stalled`: a request got no first token within
  `groups.stall_timeout` and a check through the head failed too. CapyCTL
  stops the group.
- `host_tuning_warning:<item>`: a machine's check found a gap (`compaction`,
  `memlock` or `infiniband`); the group still runs.

A deployment on one machine can show these codes the same way:

- `wake_mismatch`: after a wake, the model answered a fixed prompt
  differently than when it first became ready, so its weights did not come
  back intact. The wake fails and CapyCTL stops the instance;
  `capyctl status deployment <name>` shows the code as its last error. The
  next request or `capyctl start deployment <name>` loads it fresh.
- `wake_reload_failed` (vLLM): during a wake, the engine reported that
  reloading the model's weights failed. The wake fails and CapyCTL stops the
  instance, as for `wake_mismatch`; the next request or
  `capyctl start deployment <name>` loads it fresh. The engine log
  ([engine logs](../operations/install.md#engine-logs-and-troubleshooting))
  has vLLM's reason.
- `wake_uncertain`: a wake did not finish within the deployment's wake
  window (`timeouts.wake`), or CapyCTL could not tell whether one of its steps
  took effect (the engine stopped answering, or did not answer the fixed
  prompt). The model is never served half-woken: CapyCTL stops the instance,
  keeping its memory charged until the engine is proven gone, and the next
  request loads it fresh. A wake that keeps timing out on a large model may
  need a longer `timeouts.wake` in the deployment (as in the
  [example deployment](../examples/deployment-single.yaml)).
- `engine_config_file` (llama.cpp): `/etc/llama.cpp/config.ini` exists on the
  machine, so llama-server would read options CapyCTL cannot check. Nothing
  starts until the file is removed.
- `effective_args_mismatch` (llama.cpp): once started, llama-server reported a
  different slot count or window, or its `/metrics` or `/slots` page off, than
  CapyCTL started it with. CapyCTL stops it. Report it: the build behaves
  differently from llama.cpp v0.6.0.
- `custom_listener_exposed` (custom engine): once the engine passed its
  check, one of its processes listened on an address other than `127.0.0.1`
  or `::1`; the message names the address and port. CapyCTL stops the engine
  and the start fails. Make the manifest's `args` bind the engine to
  `"{host}"` ([The listener check](engines.md#the-listener-check)).
- `engine_listener_exposed` (a model on several GPUs): once the engine was
  ready, or when CapyCTL checked it again later, one of its processes
  listened on an address other than `127.0.0.1` or `::1`; the message names
  the process, the address and the port. CapyCTL stops the engine and the
  start fails. Report it: the engine build opened a listener CapyCTL does not
  expect.
- `tp_rank_unobserved` (a model on several GPUs): the engine did not show one
  process of its own on each of its GPUs within the start window
  (`timeouts.initialize`). The start fails and nothing is served half-started.
- `engine_rank_exited` (a model on several GPUs): one of the engine's rank
  processes exited; the message names the rank and its GPU. CapyCTL stops the
  whole model, which reads `failed`, and keeps every GPU charged until all its
  processes are gone. The next request or `capyctl start deployment <name>`
  starts it on all its GPUs again.
- `tp_stalled` (a model on several GPUs): a request got no first output within
  `groups.stall_timeout` (120 s) and a check request failed too, which is what
  a rank stuck between the GPUs looks like. CapyCTL stops the model.

The inference endpoint answers HTTP errors with a JSON body holding `code` and
`message` ([Make a request](requests.md); waiting errors are listed under
[Errors while waiting](parking.md#errors-while-waiting)).

- `engine_error` (502): the engine answered the request with an error, for
  example llama.cpp refusing an input longer than its batch; the message is
  the engine's and `retryable` is `false`. On a stream it arrives as an error
  event. The request is not sent to another instance, and it counts against
  the model's 32 running requests until the engine reports it is idle.
- `engine_error` (500) with the message `backend completion unverified`:
  CapyCTL could not tell what the engine did with the request (the connection
  failed, or the answer was cut short or unreadable). The request stays
  counted until CapyCTL has evidence that it ended, so many of these in a row
  can fill the model's running requests (`429 queue_full`). Requests cut
  before the engine answered them fill those places the same way until the
  engine's next stop; when they alone fill all 32, CapyCTL restarts the
  instance instead ([When a request is cut short](requests.md#when-a-request-is-cut-short)),
  but not while an unverified request is among them.

For decision models:

- `api_mismatch` (400): the request went to the endpoint the model does not
  answer: a chat model asked on `POST /v1/systemone`, or a decision model
  (`api: decisions`) asked on `POST /v1/chat/completions`. The message names
  the endpoint to use; nothing was woken or sent. `GET /v1/models` shows each
  model's API under `capyctl.api`
  ([Decision requests](requests.md#decision-requests)).
- `invalid_request` (400) on `POST /v1/systemone`: the body is outside the
  decision request's fields or shapes; the message says which.
- `engine_rejected` (400, 413 or 422): the engine refused the request as
  invalid, for example a `choice` question without options; the message is
  the engine's. Some custom engines answer every failure this way, an engine
  failure included: when a request you believe valid is rejected, read the
  instance's engine log ([Example: basal decision models](engines.md#example-basal-decision-models)).

## LMCache

`capyctl status deployment <name>` shows `LMCache   off: <why>` for an
instance that runs without LMCache, with the reason in the JSON view, and the
event `lmcache_off` (on `/management/v1/events`) records the instance, its
launch and the reason once per launch. A deployment with `lmcache: auto` (the
default) then runs as without LMCache; one with `lmcache: required` is refused
or its launch fails with `lmcache_unavailable:<reason>` (exit 5). The reasons:

- `disabled`: the deployment states `lmcache: off`.
- `host_off`: the machine sets `lmcache: off`. Use another machine, or set it
  back to `auto` there.
- `retention_zero`: the deployment or the machine sets `retention: zero`
  (`retention_zero:lmcache` under `required`).
- `host_capability_missing`: the machine runs a CapyCTL too old for LMCache
  (`host_capability_missing:lmcache` under `required`). Upgrade it.
- `engine_unsupported`: llama.cpp, TensorFold or a custom engine, which have
  no LMCache integration. Use vLLM or SGLang. On SGLang, also a decision
  deployment (`api: decisions`) or one whose arguments state
  `--disable-radix-cache`: SGLang fails with LMCache while its radix cache is
  disabled ([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)),
  so these run without it until a release with the fix is qualified.
- `engine_version_unsupported`: vLLM before 0.32.0 or SGLang before 0.5.22.
  Register a newer engine.
- `not_installed`: the engine's environment holds no LMCache. Install
  LMCache 0.5.6 there and register the engine again with `capyctl engine add`.
- `not_registered`: LMCache was installed after the engine was registered.
  Register the engine again.
- `version_unsupported`: LMCache below 0.5.6, or not a 0.5.x release.
  Install 0.5.6 or a later 0.5.x release and register the engine again.
- `check_failed`: the launch-time check of the LMCache installation found
  something missing; the detail names it. Reinstall LMCache in the engine's
  environment and register the engine again.
- `group_unsupported`: the deployment has a `topology` across machines; a
  group does not use LMCache.
- `multi_gpu_unqualified`: the model runs on several GPUs of one machine;
  LMCache is not yet qualified there. Under `topology: auto`, `lmcache:
  required` keeps the model on one GPU instead.
- `park_unqualified`: the deployment parks with `residency: deep` (or
  `host_backed` on a discrete GPU) on an engine version, model and GPU not
  qualified for LMCache. Use `residency: restart_only` to keep LMCache.
- `kv_transfer_config_set`: the deployment passes its own
  `--kv-transfer-config`; the engine takes one connector. Remove it to use
  LMCache.
- `server_start_failed`: the LMCache server did not answer within its start
  bound; the engine started without it. Read the engine log.
- `listener_exposed`: the LMCache server listened on an address other than
  `127.0.0.1` or `::1`; CapyCTL stopped it and the engine started without it.
- `engine_start_failed`: the engine did not start with LMCache; CapyCTL
  started it once more without LMCache, and later launches of the same
  revision on the same engine skip LMCache. The detail holds the end of the
  engine log.

The event `lmcache_server_exited` records that an instance's LMCache server
exited while its engine served. The instance fails, CapyCTL stops its engine
and recovers it with a new server, whose cache starts empty.

LMCache settings a deployment or a machine cannot state are refused
`invalid_config` (exit 2): an `lmcache_pool` that is not a whole number of GiB
of at least 1 GiB or that sits beside `lmcache: off`; an LMCache
`--kv-transfer-config`, `--enable-lmcache` or `--lmcache-*` in `extra_args`
(CapyCTL renders these itself); on a machine, `retention: zero` with
`engine_logs: files`, or an `lmcache` other than `auto` or `off`.

A model whose `lmcache_pool` does not fit the machine's host-KV limit, with
the pools of the models already running there, is not started: the start
exits 4 (`insufficient_resources`) and the message says `host_kv`, what the
model needs, what other models already hold there, the limit and the setting
to raise, for example
`host_kv (needs 20.0 GiB, limit 12.2 GiB; raise host.resource_policy.memory.system.host_kv_limit or lower lmcache_pool)`.
On a machine of several it names that machine's
`resource_policy.domains.<domain>.host_kv_limit` instead. A second copy of a
model (`instances`) that waits for that room shows the same words in its
`LAST ERROR`.

## Tracing

`capyctl status deployment <name>` shows `engine tracing: off (<reason>)` for
an instance whose engine runs without its own spans (`engine_tracing` with
`state: off` and `reason` in JSON). It is a status, not an error: the model
serves, and CapyCTL's own spans are recorded as usual. The reasons
([Engine tracing](engines.md#engine-tracing)):

- `host_off`: the machine's `observability.tracing.engines` is `off` (the
  default) or it names no endpoint.
- `engine_unsupported`: llama.cpp, TensorFold, a custom engine, vLLM before
  0.31.0 or SGLang before 0.5.22.
- `group`: the model runs across machines.
- `retention_zero`: the deployment or the machine sets `retention: zero`.
- `not_installed`: the engine's environment lacks the OpenTelemetry packages a
  traced engine imports. Install them there (for SGLang, its `tracing` extra).

An engine tracing option in `extra_args` or a profile's arguments is refused
with ``reserved option `<option>` is rendered by capyctl and cannot be
passed``, whatever the host approves: vLLM `--otlp-traces-endpoint` and
`--collect-detailed-traces`, SGLang `--enable-trace`,
`--otlp-traces-endpoint`, `--otlp-service-name` and `--trace-modules`, in
every spelling. A deployment that relied on an approved
`--otlp-traces-endpoint` before this release is refused the same way: remove
it and set `observability.tracing.engines: on` on the host.

These tracing settings refuse the role's start, with a message naming the
field ([Tracing](../operations/configuration.md#tracing)):

- a field `observability.tracing` does not take (a standalone
  `host.observability.tracing` takes only `engines`);
- an `endpoint` that is not an `http://` or `https://` URL naming a host, or
  one with a user or password in it;
- a relative `headers_file`, or one without an `endpoint`;
- a `headers_file` that cannot be opened, is a symbolic link or not a regular
  file, is owned by another user, has group or other permissions, is larger
  than 64 KiB, or has a line that is not `Name: value` (the message gives the
  line number, never its text);
- a `sample_ratio` that is not a number from 0.0 to 1.0;
- a `caller_context` other than `parent`, `link` or `ignore`;
- an `engines` other than `off` or `on`, or `engines: on` without an endpoint
  on loopback (`localhost`, `127.0.0.0/8` or `::1`).

`tracing: on → <endpoint>, spans dropped <n>, last export error: <error>`
in `capyctl status deployment <name>` means the collector could not take everything. Requests
are not affected. Check that the collector runs and that the endpoint and
`headers_file` are right; the role log has a notice at most once a minute.

## Troubleshooting

**A model does not start.** `capyctl status deployment <name>` shows it in
`LAST OPERATION`, for example `initialize failed (launch_failed)`, and the
instance's `LAST ERROR` column gives the reason, for example
`launch_failed: launch failed: engine launch failed: the engine exited before readiness`.
Each launch writes
`~/.local/state/capyctl/logs/<deployment id>/<launch id>.log` (under the host's
state directory on a GPU machine of several). It holds the engine's own output
at its default level, with keys and other credentials replaced by `<redacted>`
and no prompts, and the management API returns its end
(`GET /management/v1/deployments/<id>/engine-log`,
[engine logs](../operations/install.md#engine-logs-and-troubleshooting)).

**A model stays queued.** `status deployment` shows why under its table, for
example `Waiting     initialize pending: gave up: resource or evidence check failed: insufficient_memory: needs 109.0 GiB of system memory, 118.0 GiB available and a 11.0 GiB free reserve to keep, 2.0 GiB short`:
the machine does not have that much memory available now (on a GPU,
`insufficient_device_memory`). Stop or park another model, free the memory from
other programs, or lower the managed limit or free reserve
([memory limits](../operations/configuration.md#standalone-memory-limits)).
`start --wait` ends with `insufficient_resources` (exit 4).

**The checkpoint could not be measured.** `status deployment` shows
`Checkpoint  could not be measured (<reason>)` and `start --wait` stops at once.
`invalid_root`: the model path is not a directory CapyCTL can read, or it sits in
a directory other users can write. `unsafe_file`: a link leaves the model's
directory (or its Hugging Face cache). Fix the path or the permissions and
deploy again.

**A launch refused `checkpoint_mismatch`.** The checkpoint's digest is not the
one the deployment declares in `model.content_fingerprint`, or the one CapyCTL
recorded. A `sha256:` value with 64 hex digits there is CapyCTL's checkpoint
digest, taken over every file under the model directory, not the hash of a
weight file. `capyctl status deployment <name>` shows the declared and measured
digests; set the field to the measured one, or leave it out and CapyCTL
measures it. If the files changed on the host, restore them or deploy again.

**A start refused `checkpoint_unusable` (exit 2).** The checkpoint measured to
its digest, but the deployment's memory does not resolve with the weights it
measured, for example a `memory.request` too small to leave a KV cache once
the weights are counted. The message, and `Checkpoint  unusable` under
`capyctl status deployment <name>`, give the reason. Deploy a corrected
configuration (a larger `memory.request`, a smaller `kv_cache` or
`context_length`, or no `memory.request` so CapyCTL sizes it). A card too
small for the weights is refused `insufficient_device_memory` (exit 4)
instead.

**A launch refused `port_conflict`.** Another program listens on the engine port
CapyCTL leased. Start again (the next start takes a free port), or give CapyCTL
another range with `--engine-ports`.

**SGLang: a permissions warning in the engine log.** CapyCTL checks that the SGLang
library it uses to park and wake is not writable by other users. When the
check cannot prove that (for example, a group-writable environment on a
machine whose groups come from a directory service), CapyCTL parks anyway and
writes one warning to the engine log:
`{"event":"capyctl_saver_library_permissions","problem":"group_undetermined","action":"warned"}`.
To clear it, remove group and
other write permission from the engine's environment
(`chmod -R go-w <environment>`).

**Requests answer `401`.** The request has no API key or the wrong one. Read
it again from the credentials file ([Make a request](requests.md#the-api-key)).

**A request answers `activation_failed`.** The model could not start; see the
first item. Each new request tries a fresh start.

**After upgrading, a model is stopped.** On a discrete card, the first start
of 0.1.0 stops the engines an earlier release started and re-sizes their
deployments; the next request starts each again
([Install](install.md#upgrade)).
