# Status: llama.cpp v0.9.0 integration — 2026-11-10 (branch `engines/llamacpp-0.9.0`)

Owner decision 2026-11-10: check and update the latest engine versions. ADR 0029 §2 and §6: recognize release-tag commits for v0.7.0 and v0.9.0, and reserve the new tunnel and router controls. T42/T14/T37 refusal coverage includes underscores, equals spellings, final-vector injection and environment inputs. T42/T43 HTTP and decision fixtures cover the v0.9.0 contract and concurrent multi-question requests. The guarded live build script accepts v0.9.0 and documents CUDA architecture selection for RTX PRO 6000 versus GB10. MiniCPM OCR decisions remain explicitly unsupported because their image probe contract differs.

CPU and Fake-engine tests are not qualification. Live qualification is still owed on the rented RTX PRO 6000 x86 machine and host A, including CUDA loading/offload, readiness, chat and decision probes, streaming, cancellation and idle metrics. No VERIFIED registration or passed certification is added.

# Release note: Engines

- Recognize llama.cpp v0.9.0 and v0.7.0 release-tag builds, refuse tunnel and router controls, and document v0.9.0 installation. MiniCPM OCR decision models remain unsupported; native live qualification is still pending.
