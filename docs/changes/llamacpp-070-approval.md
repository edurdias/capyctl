# Status: llama.cpp v0.7.0 build approved on both hosts — 2026-10-16 (branch `feat/nemotron-3.5-llamacpp-070`)

Owner approval 2026-10-16: a llama.cpp v0.7.0 build (tag commit `620d222`) in the home
directory of host A and host B, for Nemotron 3.5 Bolt 74B-A9B 24GB and Nemotron 3.5 Super VL
120B-A12B. v0.7.0 adds both architectures and the LiLiCorr drafter; v0.6.0 has neither.
AGENTS.md lists the build. `scripts/live/llamacpp-build.sh` takes `LLAMACPP_TAG=v0.7.0`
and checks the tag's commit as it does for v0.6.0; any other tag is refused. Built on both
hosts on 2026-10-16: `version: 0.7.0 (build 1, commit 620d222)`, fingerprint `0.7.0+620d222`,
byte-identical binaries and libraries on the two hosts (`llama-server` sha256 `af2ea2c8…`).
CapyCTL 0.1.3 registers it unchanged (custom, as every llama.cpp build) and runs Bolt and Super VL
with stated `resources`, the LiLiCorr drafter through the approved `--spec-draft-model`, vision
through `llamacpp.mmproj_file`; text, tool calls, images and streaming pass through the router.

# Release note: none
