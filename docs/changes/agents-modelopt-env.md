# Status: A ModelOpt environment on host A for checkpoint conversion — 2026-11-03 (branch `docs/agents-modelopt-env`)

Owner decision 2026-11-03: host A gets a ModelOpt 0.47.0 environment in the home
directory (`~/modelopt-0.47.0-venv`: nvidia-modelopt 0.47.0, torch 2.13.0 cu130,
transformers 5.14.1, with a ModelOpt 0.47.0 source checkout in
`~/Model-Optimizer-0.47.0` for `examples/hf_ptq`). It converts checkpoints for
capyctl-recipes, first a Qwen3.8-27B NVFP4 checkpoint with every large Linear in
NVFP4 (the DeltaNet and attention projections that `nvidia/Qwen3.8-27B-NVFP4`
keeps in FP8 included). It is not an engine, and no engine environment changed.
AGENTS.md lists it with the other approved environments. No code changed; there
are no tests and nothing to qualify.

# Release note: none
