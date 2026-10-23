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
| 2 | `group_placement_required` | A deployment with a `topology` does not list its machines in `placement.hosts`, or also sets `host`, `selector`, `strategy` or `max_per_host`, or runs on a single-machine (standalone) role. | List the machines in `placement.hosts`, head first, and remove the other placement fields ([One model across machines](several-machines.md#one-model-across-machines)). |
| 2 | `group_topology_invalid` | `placement.hosts` repeats a machine, a dimension of `topology` is zero, or the number of machines does not divide `tensor_parallel` × `pipeline_parallel`. | Fix `topology` or `placement.hosts`. |
| 2 | `group_profile_mismatch` | The engine profile does not exist on one of the machines, or its build differs between them. | Register the same engine build under the same name on every machine. |
| 2 | `group_checkpoint_mismatch` | The model's files differ between the machines; the message lists each machine's digest. | Make the copies identical, or let CapyCTL download the model on each machine. |
| 2 | `group_model_path_mismatch` | A SGLang group with `residency: deep` has the model at different paths on its machines; the message names both. | Put the model at the same path on every machine, or use `residency: restart_only`. |
| 2 | `peer_address_missing` | A machine named in `placement.hosts` declares no peer address. | Set `resource_policy.groups.peer_address` (or `--peer-address`) on that machine and restart CapyCTL there. |
| 2 | `peer_address_not_local` | The declared peer address is not on any network interface of that machine. | Declare an address the machine holds on its direct link. |
| 2 | `engine_env_reserved:<name>` | An engine environment variable CapyCTL sets itself (for example `NCCL_*`, `GLOO_*`, `MASTER_*`, `VLLM_HOST_IP`, `CUDA_VISIBLE_DEVICES`). | Remove it; CapyCTL renders these names ([the full list](../operations/configuration.md#engine-environment)). |
| 2 | `engine_env_not_approved:<name>` | A deployment sets an engine variable the engine profile does not approve (on some machine of a group). | Approve it with `capyctl engine add ... --approve-env <name>` on every machine, or remove it ([Engine variables](engines.md#engine-variables)). |
| 2 | `engine_env_conflict:<name>` | The same variable is in the file and on the command line. | Give it one way only. |
| 2 | `custom_field_unsupported:<field>` | A deployment on a custom engine states something the engine's manifest owns: `extra_args`, `accept_extra_args`, a tool-call or reasoning parser, `memory`, a draft model or a typed engine field such as `dtype` or `max_concurrent_requests`. | Remove it; a custom engine's deployment states only `engine_config.context_length` and `engine_config.env` ([Custom engines](engines.md#custom-engines)). |
| 2 | `custom_placeholder_unset:context_length` | The custom engine's manifest uses `{context_length}` and the deployment states no `engine_config.context_length`. | Add `engine_config.context_length` to the deployment. |
| 2 | `custom_value_invalid:<placeholder>` | A deployment on a custom engine gives `{served_name}` (its first route) or `{model_dir}` (its model path) a value that starts with `-` or holds a control character, so the engine could read it as an option. | Rename the route or move the model; a deployment cannot add options to a custom engine's command. |
| 2 | `custom_credential_ref_missing` | A custom engine profile written by hand states no `security.credential_ref`, or a blank one. | Add `credential_ref: "secret://engine-key"` under the profile's `security` ([Custom engines](../operations/configuration.md#custom-engines)). |
| 2 | `declined` | `capyctl engine add --custom` asked for confirmation and the answer was not `y` or `yes`. Nothing was written. | Run it again and answer `y`, after checking what it prints. |
| 3 | `unauthorized` | The credentials were refused. | Check the admin token or the host's identity. |
| 4 | `insufficient_resources` | No host has the GPU memory the deployment needs. The message names each host's limit, for example `host a needs 64.0 GiB of gpu0 memory, 60.8 GiB free of its 60.8 GiB limit`. | Stop or park another deployment, start with `--evict`, or lower the deployment's memory. |
| 4 | `insufficient_device_memory` | The card cannot hold the model, or another program holds its memory now. | Use a smaller or quantized checkpoint, a smaller KV cache, or free the card. vLLM needs a card of about 10 GiB or more. |
| 4 | `device_unobserved` | `nvidia-smi` gave no fresh reading for that GPU, so nothing new starts on it. | Check that `nvidia-smi` works on that machine, then retry. |
| 4 | `rendezvous_ports_exhausted` | Every rendezvous port of the head machine's range is held by another group. | Stop a group on that machine, or widen `--rendezvous-ports`. |
| 4 | `rendezvous_port_in_use:<port>` | Another program holds the rendezvous port on the head machine. CapyCTL picks another port on the next start. | Retry; if it persists, find the program holding the port. |
| 4 | `service_port_in_use:<port>` | Another program holds the head's engine port, or a SGLang worker's local port. | Retry; if it persists, find the program holding the port. |
| 4 | `host_tuning_missing:<item>` | With `require_rdma: true`, a machine lacks `memlock` (unlimited locked memory) or `infiniband` (read-write `/dev/infiniband/uverbs*`). | Fix the item as root on that machine, or set `require_rdma: false`. |
| 5 | `unsupported` | The request is valid but this release cannot do it. | Read the message; it names the missing capability. |
| 5 | `store_from_newer_version` | The state directory was written by a newer CapyCTL. | Run the newer binary, or restore a backup taken before the upgrade. |
| 5 | `multi_gpu_unsupported` | The deployment names two GPUs on one machine. | Use one GPU per machine; to spread one model over machines, give it a `topology` and `placement.hosts`. |
| 5 | `unsupported_gpu_topology` | The machine has both an integrated and a discrete GPU. | Not supported in this release. |
| 5 | `host_backed_unavailable` | `residency: host_backed` on unified memory. | Use `deep`, or leave `residency` out. |
| 5 | `capability_missing`, `capability_missing:deep_park` | `--deep-park enabled` on an engine that cannot park (TensorFold, llama.cpp, a custom engine), or a deployment asking such an engine to park (`residency: deep` or `host_backed`). | Leave `--deep-park` out; use `residency: restart_only`. |
| 5 | `group_shape_unsupported`, `group_shape_unsupported:<engine>` | More than one rank per machine, or a shape the engine cannot run (TensorFold runs `tensor_parallel: 2` on exactly two machines; a custom engine, `group_shape_unsupported:custom`, runs on one machine only). | Use one machine per rank and a shape the engine supports ([Groups across machines](engines.md#groups-across-machines)); for a custom engine, remove `topology` and use `instances` for more copies. |
| 5 | `group_instances_unsupported` | `instances` above 1 with a `topology`. | Deploy one group per deployment. |
| 5 | `group_drift:<field>` | The engine changed a multi-machine setting CapyCTL rendered (the field is named). | Report it; the engine build behaves differently from the one CapyCTL supports. |
| 5 | `host_capability_missing:engine_groups` | A machine runs a CapyCTL too old for groups. | Upgrade CapyCTL on that machine. |
| 5 | `capability_missing:decisions` | A deployment with `api: decisions` names an engine that cannot serve decision models: vLLM (any version: 0.32.0 answers Clef and Clef-Flash from their text output instead of their decision head), TensorFold, llama.cpp before 0.7.0, SGLang before 0.5.22, an installation whose registered build names no version, or a custom engine whose manifest says `api: chat`. | Use llama.cpp 0.7.0 or newer or SGLang 0.5.22 or newer, registered with `capyctl engine add`, or a custom engine with `api: decisions` ([Decision models](deploy.md#decision-models)). |
| 5 | `capability_missing:chat` | A chat deployment (no `api`, or `api: chat`) names a custom engine whose manifest says `api: decisions`. | Add `api: decisions` to the deployment, or use a chat engine. |
| 5 | `host_capability_missing:decisions` | A machine runs a CapyCTL too old for decision models. | Upgrade CapyCTL on that machine. |
| 5 | `host_capability_missing:custom_engines` | The deployment's engine is a custom engine and a machine it names runs a CapyCTL too old for custom engines. | Upgrade CapyCTL on that machine. |
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
