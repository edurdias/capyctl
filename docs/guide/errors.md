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
| 5 | `capability_missing` | `--deep-park enabled` on an engine that cannot park (TensorFold), or a deployment asking such an engine to park. | Leave `--deep-park` out; use `residency: restart_only`. |
| 5 | `group_shape_unsupported`, `group_shape_unsupported:<engine>` | More than one rank per machine, or a shape the engine cannot run (TensorFold runs `tensor_parallel: 2` on exactly two machines). | Use one machine per rank and a shape the engine supports ([Groups across machines](engines.md#groups-across-machines)). |
| 5 | `group_instances_unsupported` | `instances` above 1 with a `topology`. | Deploy one group per deployment. |
| 5 | `group_drift:<field>` | The engine changed a multi-machine setting CapyCTL rendered (the field is named). | Report it; the engine build behaves differently from the one CapyCTL supports. |
| 5 | `host_capability_missing:engine_groups` | A machine runs a CapyCTL too old for groups. | Upgrade CapyCTL on that machine. |
| 6 | `unreconciled` | CapyCTL cannot yet tell what an engine is doing, so it will not act on it. | Wait for the host to reconnect, then retry. |
| 7 | `device_conflict` | Another deployment holds the GPU exclusively. | Stop that deployment or choose another device. |
| 8 | `category_limit` | A host limit on how many models of this kind may run was reached. | Stop one, or raise the limit in the host document. |
| 10 | `activation_timeout` | The engine did not become ready in time. | Check the engine log; for large models raise the deployment's `timeouts.initialize`, or pass `--initialize-timeout` to `capyctl start deployment`. |
| 11 | `topology_unknown` | The host has not reported its GPUs yet. | Wait for the host to finish starting, then retry. |
| 12 | `no_safe_estimate` | CapyCTL cannot estimate the model's memory safely. | Declare the deployment's resources explicitly. |
| 13 | `internal` | An internal, storage or I/O failure, or a launch that failed (`operation_failed`). | Read the message, `capyctl status deployment <name>` and the role's log. |
| 14 | `host_revoked` | The server revoked this host. The host exits and is not restarted. | Bring it back (see [Several machines](several-machines.md#take-a-machine-out)). |
| 15 | `host_ineligible` | No allowed host can take the deployment: drained, revoked, offline or too old. | Undrain, reconnect, upgrade or re-enroll the host the message names. |
| 16 | `engine_not_found` | The path holds no `vllm`, `sglang` or `tensorfold` package. | Name the environment, its `bin/vllm`, `bin/tensorfold` or `bin/python3`, or search with `capyctl engine detect --path <dir>`. |
| 17 | `engine_unsupported` | The package is not a supported engine. | Register a vLLM, SGLang or TensorFold installation. |
| 18 | `engine_version_failed` | The engine's version check failed or timed out; nothing was written. | Repair the installation until its version check succeeds, then add it again. |
| 19 | `profile_exists` | The engine profile name is taken. | Pass `--name`, or remove the existing profile first. |
| 20 | `profile_in_use` | Removing or replacing the engine would affect the deployments listed. | Stop them, or rerun with `--drain`. |
| 21 | `publish_rejected` | The server refused the updated engine list; the reason follows. | Fix what the reason names. |
| 22 | `agent_unreachable` | CapyCTL on this machine is running but did not answer an engine command. | Retry; if the message says the outcome is unknown, run `capyctl engine list` first. |
| 23 | `not_interactive` | `capyctl engine add` without a path needs a terminal to pick one. | Name the installation. |
| 24 | `profile_not_published` | No allowed host offers the engine profile the deployment names; nothing was stored. | Register the engine on a host with `capyctl engine add <path> --name <profile>`, then deploy again. |
| 25 | `still_stopping` | A `start` came right after a `stop`, before the engine finished going away, or while CapyCTL was still confirming that a slow stop's engine exited; nothing was started. | Retry in a moment, or run `capyctl start deployment <name> --wait`, which waits for the stop and then starts. |
| 26 | `toolchain_missing` | `capyctl engine add` found a TensorFold installation, but `ninja`, `nvcc` or a C++ compiler is not on the engine's PATH (its `bin`, the CUDA toolkit's `bin`, then `/usr/local/bin`, `/usr/bin`, `/bin`). TensorFold builds CUDA kernels on its first start. Nothing was written. | Install what the message names into one of those directories, or set `CUDA_HOME` to a toolkit that has `nvcc`, then add the engine again. |

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

A deployment on one machine can show this code the same way:

- `wake_mismatch`: after a wake, the model answered a fixed prompt
  differently than when it first became ready, so its weights did not come
  back intact. The wake fails and CapyCTL stops the instance;
  `capyctl status deployment <name>` shows the code as its last error. The
  next request or `capyctl start deployment <name>` loads it fresh.

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
