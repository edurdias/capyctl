# Status: Inkling 2 Mini engine environment approved on host A — 2026-11-20 (branch `docs/approve-inkling-env`)

Owner decision, 2026-11-20: host A may get a new engine environment for
Thinking Machines' Inkling 2 Mini, SGLang built from PR #51926 (commit
`92e894ef`, the first SGLang source with this model and SM121 kernels), in
`~/sglang-inkling-pr51926-venv`, made by the capyctl-recipes Inkling
comparison's `install.sh` with its Rust toolchain and caches inside the venv.
The owner also approved a loader patch in that venv only, so attention
projections can load as FP8 for a requantized checkpoint. No approved release
of SGLang, vLLM or llama.cpp supports the model yet. AGENTS.md's list of
approved environments records both. Documentation only; no code or tests
changed. CPU and Fake-engine tests are not qualification; the environment is
qualified by the comparison's live runs.

# Release note: none
