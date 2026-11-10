# Status: SGLang 0.5.23 launch policy — 2026-11-10 (branch `engines/sglang-0.5.23`)

Owner decision 2026-11-10: check latest engine versions and update them. SPEC §8.2 / T21 and ADR 0033 §11: close SGLang's HTTP application environment override before imports and reserve external cache linker enable, backend and configuration destinations, including the dynamic backend. The optional destinations do not become required core-probe fields on older engines. The removed `speculative_skip_dp_mlp_sync` extra produces `invalid_extra_args`, not a constructor failure. The guide gives the noneditable Python 3.12 / SGLang 0.5.23 / torch-memory-saver 0.0.10 recipe without adding a verified version.

Targeted CPU tests passed: Rust `engine_env` and `engine_config` (40 tests), Python `test_sglang_server_args` (49 tests, with `PYTHONPATH=runtime/tests`) and `test_sglang_startup_guards` (25 tests); formatting check passed. The initial combined Python invocation could not import the mapper fixture until its existing test path was supplied. CPU and Fake-engine tests are not qualification. Live native imports, multi-tokenizer workers, saver/parking/reload, TP enrollment and cache isolation still need checking on the rented RTX PRO 6000 x86 machine and host A. No qualification was marked passed.

# Release note: Engines

- **SGLang 0.5.23 policy coverage.** Alternative HTTP applications and external cache linkers are refused even with generic approval. The installation guide now gives the latest noneditable wheel recipe; live qualification remains pending.
