# Status: TensorFold native 1.2.6 compatibility — 2026-11-10 (branch `engines/tensorfold-1.2.6`)

Following the owner's latest-engine update decision, ADR 0039 gains a Proposed amendment targeting native 1.2.6 on x86_64 (sm_120) and aarch64. Native policy reserves `--unix-socket` and refuses `--prompt-cache-disk` and `--spill-gib` under zero retention. Regression coverage exercises the release's capabilities, grammar-library fingerprint, command, readiness metadata and omitted parked work gauges. The installation guide gives the x86_64 archive recipe.

CPU and Fake-engine tests are not qualification. Live checks on the rented RTX PRO 6000 x86 machine and host A remain owed; no certification is marked passed and the Python verified set is unchanged.

# Release note: Engines

- **TensorFold native 1.2.6 compatibility.** Adds archive installation instructions and compatibility coverage, reserves its Unix-socket listener option and blocks its new disk-cache options under zero retention. Live qualification remains pending.
