# Status: The new engine environments on host A, with their versions — 2026-11-10 (branch `docs/agents-env-versions`)

The environments the owner approved on 2026-11-09 were made on host A on 2026-11-10 and are
recorded in `AGENTS.md` with their exact versions: vLLM 0.33.0 (torch 2.14.0+cu130) and
llama.cpp b12242 (commit 3b4d6ab3, CUDA architecture 121a). The laptop's
`lmsysorg/sglang:v0.5.22` image was removed after #230's check and is to be pulled again
for #235's. CapyCTL 0.1.11 registers vLLM 0.33.0 but cannot launch it (its protected
runtime imports `cli_env_setup` from the module vLLM 0.33.0 moved it out of, #239), so 0.33.0
is not qualified yet.

# Release note: none
