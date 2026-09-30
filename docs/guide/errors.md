# Exit codes and errors

Every capyctl command exits 0 on success. On failure it prints one line to
standard error, `error [<code>]: <message>`, and exits with the status below.
With `--format json` the error is a JSON object with `code` and `message`.

| Exit | Code | What it means | What to do |
|---|---|---|---|
| 2 | `invalid_config` | A document or command argument is invalid. | Run `capyctl validate config --file <file>` and fix what it reports. |
| 2 | `not_found` | The deployment, instance or host named does not exist. | Check the name with `capyctl list deployments` or `capyctl list hosts`. |
| 2 | `device_policy_mismatch` | A host's file describes a card that does not match the GPU it found. | Fix the `gpuN` entries of `resource_policy` to match `nvidia-smi -L`. |
| 2 | `missing_system_allocation` | A deployment that lists its own `resources` on a discrete card leaves out host RAM (`system`). | Add the `system` entry, or leave `resources` out and let capyctl size it. |
| 3 | `unauthorized` | The credentials were refused. | Check the admin token or the host's identity. |
| 4 | `insufficient_resources` | No host has the GPU memory the deployment needs. | Stop or park another deployment, start with `--evict`, or lower the deployment's memory. |
| 4 | `insufficient_device_memory` | The card cannot hold the model, or another program holds its memory now. | Use a smaller or quantized checkpoint, a smaller KV cache, or free the card. vLLM needs a card of about 10 GiB or more. |
| 4 | `device_unobserved` | `nvidia-smi` gave no fresh reading for that GPU, so nothing new starts on it. | Check that `nvidia-smi` works on that machine, then retry. |
| 5 | `unsupported` | The request is valid but this release cannot do it. | Read the message; it names the missing capability. |
| 5 | `store_from_newer_version` | The state directory was written by a newer capyctl. | Run the newer binary, or restore a backup taken before the upgrade. |
| 5 | `multi_gpu_unsupported` | The deployment names two GPUs or asks for tensor parallelism. | Use one GPU per model. |
| 5 | `unsupported_gpu_topology` | The machine has both an integrated and a discrete GPU. | Not supported in this release. |
| 5 | `host_backed_unavailable` | `residency: host_backed` on unified memory. | Use `deep`, or leave `residency` out. |
| 6 | `unreconciled` | capyctl cannot yet tell what an engine is doing, so it will not act on it. | Wait for the host to reconnect, then retry. |
| 7 | `device_conflict` | Another deployment holds the GPU exclusively. | Stop that deployment or choose another device. |
| 8 | `category_limit` | A host limit on how many models of this kind may run was reached. | Stop one, or raise the limit in the host document. |
| 10 | `activation_timeout` | The engine did not become ready in time. | Check the engine log; for large models raise the deployment's `timeouts.initialize`, or pass `--initialize-timeout` to `capyctl start deployment`. |
| 11 | `topology_unknown` | The host has not reported its GPUs yet. | Wait for the host to finish starting, then retry. |
| 12 | `no_safe_estimate` | capyctl cannot estimate the model's memory safely. | Declare the deployment's resources explicitly. |
| 13 | `internal` | An internal, storage or I/O failure, or a launch that failed (`operation_failed`). | Read the message, `capyctl status deployment <name>` and the role's log. |
| 14 | `host_revoked` | The server revoked this host. The host exits and is not restarted. | Bring it back (see [Several machines](several-machines.md#take-a-machine-out)). |
| 15 | `host_ineligible` | No allowed host can take the deployment: drained, revoked, offline or too old. | Undrain, reconnect, upgrade or re-enroll the host the message names. |
| 16 | `engine_not_found` | The path holds no `vllm` or `sglang` package. | Name the environment, its `bin/vllm` or its `bin/python3`, or search with `capyctl engine detect --path <dir>`. |
| 17 | `engine_unsupported` | The package is not a supported engine. | Register a vLLM or SGLang installation. |
| 18 | `engine_version_failed` | The engine's version check failed or timed out; nothing was written. | Repair the installation until its version check succeeds, then add it again. |
| 19 | `profile_exists` | The engine profile name is taken. | Pass `--name`, or remove the existing profile first. |
| 20 | `profile_in_use` | Removing or replacing the engine would affect the deployments listed. | Stop them, or rerun with `--drain`. |
| 21 | `publish_rejected` | The server refused the updated engine list; the reason follows. | Fix what the reason names. |
| 22 | `agent_unreachable` | capyctl on this machine is running but did not answer an engine command. | Retry; if the message says the outcome is unknown, run `capyctl engine list` first. |
| 23 | `not_interactive` | `capyctl engine add` without a path needs a terminal to pick one. | Name the installation. |
| 24 | `profile_not_published` | No allowed host offers the engine profile the deployment names; nothing was stored. | Register the engine on a host with `capyctl engine add <path> --name <profile>`, then deploy again. |
| 25 | `still_stopping` | A `start` came right after a `stop`, before the engine finished going away; nothing was started. | Retry in a moment, or run `capyctl start deployment <name> --wait`, which waits for the stop and then starts. |

The systemd units restart capyctl when it fails, except on exits a restart cannot
fix: 2, 3 and 5, and 14 for a host.

## Troubleshooting

**A model does not start.** `capyctl status deployment <name>` shows it in
`LAST OPERATION`, for example `initialize failed (launch_failed)`, and the
instance's `LAST ERROR` column gives the reason, for example
`launch_failed: launch failed: engine launch failed: the engine exited before readiness`.
The engine's own output is
not kept by default, because it may contain secrets. To see it, restart capyctl
with `--debug-engine-logs` and start the model again; each launch then writes
`~/.local/state/capyctl/logs/<deployment id>/<launch id>.log` (under the host's
state directory on a GPU machine of several).

**SGLang: a permissions warning in the engine log.** capyctl checks that the SGLang
library it uses to park and wake is not writable by other users. When the
check cannot prove that (for example, a group-writable environment on a
machine whose groups come from a directory service), capyctl parks anyway and
writes one warning to the engine log:
`{"event":"capyctl_saver_library_permissions","problem":"group_undetermined","action":"warned"}`.
You only see it with `--debug-engine-logs`. To clear it, remove group and
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
