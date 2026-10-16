# Status: Docs show the current engine versions — 2026-10-16 (branch `docs/current-engine-versions`)

The guides still showed the engine versions of their first writing (vLLM 0.29.0, SGLang 0.5.20,
TensorFold 0.6.0 and 0.6.1) after 0.1.3 verified vLLM 0.31.0 and TensorFold 0.6.6. All three
stay supported; the docs now lead with the newest.

- **"First model on each engine" was re-run live** on host A with the published 0.1.3
  binary (isolated config, state and prefix; checkpoints from the shared cache), vLLM 0.31.0,
  SGLang 0.5.21 and TensorFold 0.6.6. Ready in 53 s, 91 s and 109 s (TensorFold restart
  after stop: 12 s); every chat answered. The output blocks are that run's. The Qwen3 answers no
  longer carry `<think></think>` in `content`: the reasoning parser CapyCTL picks moves it to
  `reasoning` (vLLM) or `reasoning_content` (SGLang). Everything was removed afterwards.
  Evidence: `target/live/docs-walkthrough-0.1.3/`.
- **Install an engine:** the vLLM steps pin 0.31.0 (0.30.0 and 0.29.0 named as alternatives,
  all on PyTorch 2.13.0) and link vLLM's 0.31.0 guide.
- **Engine options:** the vLLM table covers 0.30 and 0.31 and lists 0.31's logging options
  (`--log-level`, `--log-config-file`, `--logging-config`) as reserved, which the deploy-time
  policy already does; `engine_flag_table` checks the row.
- **Requests:** the streamed-field and per-request-metrics tables include vLLM 0.31.
- **Illustrative output** (engine detect, add and list, the example host documents'
  `build_fingerprint`): vLLM 0.31.0, SGLang 0.5.21, TensorFold 0.6.6, and a 0.32.0rc1 nightly
  for the custom-build example. The site's transcript capture uses the same versions; its
  one-machine part was rerun with the 0.1.3 binary and printed these lines.
- **Stale release references:** `multi_gpu_unsupported` no longer says "one GPU per model in
  0.1.0 ... planned after 0.1.0" (in the error and in the install guide); it points to
  `topology` for a model across machines. The `engines.yaml` compatibility note dates the
  skip of an unknown engine's profile to 0.1.2 instead of "this release".
- The groups table says its versions are the ones the two-machine live runs used.

Measured figures attributed to a version (wake times, memory, sources read) keep the version
they were measured on.

# Release note: none
