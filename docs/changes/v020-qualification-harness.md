# Status: Complete the 0.2.0 qualification harness — 2026-11-12 (branch `live/v020-all-rows`)

The 0.2.0 harness implements all previously unimplemented runnable embedding, tracing, TensorFold and LMCache rows (ADRs 0034, 0036, 0037 and 0039). TensorRT-LLM rows remain listed but skip under the ADR 0038 release hold-out; they never launch. EM6 qualifies only vLLM, per the owner's 2026-11-11 decision, and records why SGLang and llama.cpp cannot use its observational hook. The catalog preserves each row once and the README mirrors its implemented criteria.

Setup provisions only runnable selected rows' engine, model and qualification-build prerequisites. It uses session-private caches, tools and ownership receipts, preserves read-only supplied environments, and installs nothing for held-out TensorRT rows. Historical LC12 and LC18 source copies use committed zero-fuzz patches rather than changing the release executable. The integrated LC12 patch tracks the preserving-sleep guard.

The owner-approved 2026-11-12 test-only vLLM 0.34.0rc1/LMCache 0.5.7 arm uses uv overrides for OpenTelemetry API, SDK and OTLP exporter 1.45.1 and Prometheus exporter 0.66b1. This overrides LMCache's conflicting API and Prometheus caps, retains vLLM's API requirement and engine/torch constraints, and records dependency, import and bounded loopback startup/metrics evidence. It is not a normal compatible upstream dependency combination or an exemption for production. LC13–LC16, including unified-memory counterparts, retain the override receipt with their measurements.

The 2026-11-12 owner-decision amendment to ADR 0039 makes TF4 strictly require the shared product codes: `multi_gpu_unsupported` on one host and `group_shape_unsupported:tensorfold` across hosts. CPU tests reject the old or wrong-shape codes. Product code and its existing TensorFold config regression tests are unchanged. The LMCache sleep-preservation product change and TensorRT-LLM release hold-out retain their separate change files, `lmcache-sleep-preserve.md` and `tensorrt-llm-release-holdout.md`.

Verification: all 127 harness Python tests passed, ShellCheck 0.11.0 passed, and `scripts/live/v020/run.sh --dry-run` passed. CPU and Fake-engine tests are not qualification; live reruns must still establish the native engine, memory, timing and ownership criteria before certification. No native engine was launched by this integration.

# Release note: none
