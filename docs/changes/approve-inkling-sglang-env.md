# Status: Inkling 2 Mini, INT4-builder and 16 GB engine environments approved — 2026-11-21 (branch `docs/approve-inkling-env`)

Owner decision, 2026-11-20: the lab hosts may get a new engine environment for
Thinking Machines' Inkling 2 Mini, SGLang built from PR #51926 (commit
`92e894ef`, the first SGLang source with this model and SM121 kernels), in
`~/sglang-inkling-pr51926-venv`, made by the capyctl-recipes Inkling
comparison's `install.sh` with its Rust toolchain and caches inside the venv.
The owner also approved a loader patch in that venv only, so attention
projections can load as FP8 for a requantized checkpoint, and the use of vLLM's
`vllm/vllm-openai:inkling-2-mini` Docker image pinned by digest
(`sha256:c2dc759f…`, a 0.35 pre-release) to reproduce a published community
recipe as written. No approved release of SGLang, vLLM or llama.cpp supports
the model yet.

Owner decisions, 2026-11-21: the lab hosts may get an isolated llm-compressor
environment (`~/llmcompressor-venv`) as a checkpoint builder only, for INT4
checkpoints in the compressed-tensors format (ModelOpt 0.47.0's INT4 export
does not load in the approved SGLang or vLLM); and maintainers' local machines
with a 16 GB GPU may get SGLang 0.5.22 in a home-directory venv
(`~/sglang-0.5.22-venv`) for qualifying 16 GB recipes, with any other GPU
workload on that machine stopped while testing.

AGENTS.md's list of approved environments records all of them. Documentation
only; no code or tests changed. CPU and Fake-engine tests are not
qualification; each environment is qualified by the live runs that use it.

# Release note: none
