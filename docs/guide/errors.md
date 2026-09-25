# Exit codes and errors

Every mllm command exits 0 on success. On failure it prints one line to
standard error, `error [<code>]: <message>`, and exits with the status below.
With `--format json` the error is a JSON object with `code` and `message`.

| Exit | Code | What it means | What to do |
|---|---|---|---|
| 2 | `invalid_config` | A document or command argument is invalid. | Run `mllm validate config --file <file>` and fix what it reports. |
| 2 | `not_found` | The deployment, instance or host named does not exist. | Check the name with `mllm list deployments` or `mllm list hosts`. |
| 3 | `unauthorized` | The credentials were refused. | Check the admin token or the host's identity. |
| 4 | `insufficient_resources` | No host has the GPU memory the deployment needs. | Stop or park another deployment, start with `--evict`, or lower the deployment's memory. |
| 5 | `unsupported` | The request is valid but this release cannot do it. | Read the message; it names the missing capability. |
| 5 | `store_from_newer_version` | The state directory was written by a newer mllm. | Run the newer binary, or restore a backup taken before the upgrade. |
| 6 | `unreconciled` | mllm cannot yet tell what an engine is doing, so it will not act on it. | Wait for the host to reconnect, then retry. |
| 7 | `device_conflict` | Another deployment holds the GPU exclusively. | Stop that deployment or choose another device. |
| 8 | `category_limit` | A host limit on how many models of this kind may run was reached. | Stop one, or raise the limit in the host document. |
| 10 | `activation_timeout` | The engine did not become ready in time. | Check the engine log; for large models raise the deployment's `timeouts.initialize`, or pass `--initialize-timeout` to `mllm start deployment`. |
| 11 | `topology_unknown` | The host has not reported its GPUs yet. | Wait for the host to finish starting, then retry. |
| 12 | `no_safe_estimate` | mllm cannot estimate the model's memory safely. | Declare the deployment's resources explicitly. |
| 13 | `internal` | An internal, storage or I/O failure. | Read the message and the role's log. |
| 14 | `host_revoked` | The server revoked this host. The host exits and is not restarted. | Bring it back (see [Several machines](several-machines.md#take-a-machine-out)). |
| 15 | `host_ineligible` | No allowed host can take the deployment: drained, revoked, offline or too old. | Undrain, reconnect, upgrade or re-enroll the host the message names. |
| 16 | `engine_not_found` | The path holds no `vllm` or `sglang` package. | Name the environment, its `bin/vllm` or its `bin/python3`, or search with `mllm engine detect --path <dir>`. |
| 17 | `engine_unsupported` | The package is not a supported engine. | Register a vLLM or SGLang installation. |
| 18 | `engine_version_failed` | The engine's version check failed or timed out; nothing was written. | Repair the installation until its version check succeeds, then add it again. |
| 19 | `profile_exists` | The engine profile name is taken. | Pass `--name`, or remove the existing profile first. |
| 20 | `profile_in_use` | Removing or replacing the engine would affect the deployments listed. | Stop them, or rerun with `--drain`. |
| 21 | `publish_rejected` | The server refused the updated engine list; the reason follows. | Fix what the reason names. |
| 22 | `agent_unreachable` | mllm on this machine is not running, or did not answer. | Start it and retry. An engine you added takes effect when it starts. |
| 23 | `not_interactive` | `mllm engine add` without a path needs a terminal to pick one. | Name the installation. |
| 24 | `profile_not_published` | No allowed host offers the engine profile the deployment names; nothing was stored. | Register the engine on a host with `mllm engine add <path> --name <profile>`, then deploy again. |

The systemd units restart mllm when it fails, except on exits a restart cannot
fix: 2, 3 and 5, and 14 for a host.
