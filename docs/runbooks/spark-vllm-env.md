# DGX Spark — vLLM Environment Contract (operator-executed)

**mllm installs nothing** (SPEC §4.2 / T07): the operator executes the steps below on
the Spark. mllm observes the result via `mllm doctor host` and freezes the live recipe
only from reported reality (F1 design §8). Fill the `<CAPTURED>` values from doctor
output; never guess them.

## 1. vLLM install (dedicated venv)

```bash
# On the Spark, as the operator:
python3 -m venv ~/mllm-vllm-venv   # /opt requires root on the lab Spark; user-writable venv (recorded deviation)
~/mllm-vllm-venv/bin/pip install --upgrade pip
~/mllm-vllm-venv/bin/pip install vllm==<PINNED_VERSION>   # pin set at first doctor capture
~/mllm-vllm-venv/bin/vllm --version                        # record this output as the fingerprint
```

`<PINNED_VERSION>` is chosen at the first live session: install the current stable
release that builds for aarch64/GB10, run `mllm doctor host`, and record the exact
version in this file before any deploy test. If sleep mode misbehaves on the GB10
platform, revise the pin (F1 design §8 step 4 — park/reload is core functionality).

## 2. Checkpoint (pre-downloaded, read-only)

```bash
mkdir -p /srv/models
# Operator downloads the owner-approved checkpoint into /srv/models/<model-id>.
# Design proposal pending owner approval at design review: small instruct model
# (Qwen3-4B/8B class), BF16, sized to exercise ledger headroom on 128 GiB unified.
```

## 3. mllm runtime profile

```yaml
# host.yaml (excerpt) — added after the install, fingerprinted by doctor.
runtime_profiles:
  vllm-stock:
    adapter: vllm
    launch:
      type: exec
      command: ["$HOME/mllm-vllm-venv/bin/vllm", "serve"]
      argument_contract: native
```

## 4. Development-mode warning (upstream, carried verbatim)

vLLM's security documentation warns against enabling development mode in production and
identifies the collective RPC surface as dangerous [S2 — docs.vllm.ai/en/latest/usage/security].
The deep-park path (`/sleep`, `/wake_up`, `/collective_rpc`) requires development mode
and therefore **requires the host-policy opt-in
`security.allow_development_engine_controls: true`** for the `vllm-sleep` profile — the
opt-in gates the profile itself, not just the operations (F1 design §7). Private
binding and the ingress gate do not erase this warning. Production qualification
requires a separately reviewed control path.

Qualified lab pin (2026-09-12): vLLM **0.29.0**, uv-managed Python 3.12.14 in
`~/mllm-vllm-venv2`, Qwen/Qwen3-4B-Instruct-2507 BF16 in
`~/models/qwen3-4b-instruct`. Both `host-a` and `host-b` use this recipe.
The launch uses 16 GiB explicit KV cache and a 4096-token maximum context.
The sleep profile requires both `--enable-sleep-mode` and
`VLLM_SERVER_DEV_MODE=1`; stock launches explicitly set the latter to 0.
Restoration calls `/wake_up`, then `/collective_rpc` with
`{"method":"reload_weights"}`. Checkpoint reload has a 120s client timeout
(44–50s observed); sleep and wake retain 30s bounds. Full live results and
the superseded 64 GiB memory recipe are in `spark-qualification-f1.md`.

## 5. Doctor capture (recipe freeze)

```bash
mllm doctor host <host-name>   # fingerprints + memory observations
```

Record: vLLM fingerprint, checkpoint revision, observed `memory.system` bytes. The
deploy recipe is frozen only from this output.
