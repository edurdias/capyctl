# mllm 0.1.0 release notes (draft)

Draft text for the owner to copy into the GitHub release. mllm never publishes a
release itself.

## Network access

### Inference is reachable from other machines

The inference endpoint now listens on all interfaces, `0.0.0.0:8443`, and still
requires the API key. An existing configuration that used the old default
(`127.0.0.1:8443`) is updated once at the first start, with a copy of the old file
kept as `<file>.pre-0.1.0` and a notice printed. To keep inference on this
machine only, start with `--listen 127.0.0.1:8443` or set
`listeners.inference.bind` in the configuration; to limit it to your tailnet, use
the machine's Tailscale address. See docs/operations/network-access.md.

- The same address can be set with `MLLM_INFERENCE_ADDR`, for both the server and
  standalone (`MLLM_STANDALONE_INFERENCE_ADDR` still works, with a warning).
- The key can be turned off only explicitly: `listeners.inference.authentication:
  none`, `--no-inference-auth` or `MLLM_INFERENCE_AUTH=none`. On an address other
  machines can reach, the start prints a warning and `mllm status` repeats it.
- The fixed fallback key `mllm-local` is gone. If the credentials file cannot be
  read, standalone refuses to start instead of serving with a known key.
- If the configuration file cannot be rewritten (for example a server document
  in a read-only `/etc/mllm`), it is left unchanged and every start that still
  finds the old default serves on `0.0.0.0:8443` and prints
  `config_migration_failed`; set `listeners.inference.bind` there (or start
  with `--listen`) to choose another address.
- Management stays on loopback with its admin token, and engines stay on
  loopback behind per-launch keys. mllm does not terminate TLS; for the internet,
  put a TLS reverse proxy in front (a Caddy example is in the network access
  guide).

## Discrete NVIDIA GPUs

mllm now runs on machines whose GPU has its own memory, such as GeForce and RTX
cards, as well as on unified-memory machines like the GB10.

- **Device memory is accounted.** Each GPU's memory is its own domain, read from
  `nvidia-smi`, next to host RAM. Every deployment is charged on both, so mllm
  parks or stops a model before a second one would overfill the card, and the
  launch check reads the card's real free memory. A GPU without a fresh reading
  takes no new work (`device_unobserved`).
- **Host-RAM parking.** A new `host_backed` residency parks a model by copying
  its weights to pinned host RAM and wakes it by copying them back, much faster
  than reloading from disk. It is the default on a discrete GPU when the copy fits
  the host-RAM parked limit, otherwise `deep`. vLLM uses sleep level 1, SGLang
  its weights CPU backup. When the copy no longer fits, the model is stopped
  instead of parked, and the switch record says so. SGLang ModelOpt (NVFP4)
  checkpoints cannot park yet; deploy them `restart_only`.
- **mllm picks the GPU.** On a machine with several GPUs, each model goes on the
  GPU with room, evicting only on that GPU when needed. Pin one with
  `devices: [{id: gpu1}]` (the GPU's sharing is the host's unless stated). One GPU per model in 0.1.0: a
  deployment naming two GPUs, or tensor parallelism, is refused
  (`multi_gpu_unsupported`).
- **Sizing from the checkpoint.** A deployment that states no memory asks the card
  for its weights plus 10 % and a KV cache of up to 4 GiB; a vLLM deployment
  asks for at least 75 % of the card. A model too large for the card is refused at
  deploy (`insufficient_device_memory`).
- **Enrolled hosts** report device memory through a new capability,
  `device_memory_domains`; upgrade the server first, then the hosts. Unified
  hosts are unaffected.

Tests on CPU and with a fake engine pin the accounting and configuration; they
are not evidence that a native engine recipe works on a given card.

## Other defaults in 0.1.0

- A deployment file needs only `name`, `engine` and `model`; mllm fills in the
  rest.
- Models live in `~/models` unless configured otherwise, and Hugging Face and HTTP
  downloads are allowed by default into `~/models/sources`, capped at 500 GiB with
  1 GiB of disk kept free.
- Every setting can be stated in the YAML document, with a flag, or with an
  environment variable, and any document setting with `--set path=value` or
  `MLLM_SET__PATH=value`; `mllm config show` prints each effective value and
  where it came from. See docs/operations/configuration.md.
