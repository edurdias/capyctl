# Status: Restart replaced queued deployments — 2026-11-13 (branch `fix/start-after-replace`)

ADR 0013 §7 (owner decision Q8), SPEC §§6.3 and 13.2: replacing a deployment whose Initialize stayed queued after a memory-admission refusal must retire that obsolete plan before starting the replacement. Instance reconciliation previously waited for every queued activation to finish, including one whose own admission had closed; another start joined the closed plan and reported `reconciliation_required`, misleadingly suggesting an unavailable resource policy.

Only a queued ordinary Initialize with a planned step, no grant and no execution enters the existing verified unarmed-stop path. That path proves no effects and retains its binding, lease and claim until the owned worker has awaited the predecessor's exit and completes the stop. Armed or uncertain activations continue waiting for evidence. Starts during replacement cleanup report retained ownership rather than joining an obsolete plan. Stale lifecycle failures identify changed lifecycle state or coordinator session instead of a missing resource policy.

T08/T10/T16/T30/T33 regressions cover the insufficient-device-memory refusal followed by replacement and successful Fake initialization, plus retention until unarmed-stop completion and the unchanged armed-launch guard. CPU and Fake-engine tests are not native-engine qualification; the original discrete-GPU standalone scenario still needs a live recheck after the fix is installed.

The Fake management regression failed with the original `reconciliation_required` response on both `feat/web-ui` at `59e8fc49` and `origin/main` at `4b08b1a4`, then passed with this fix. Verification passed: store unit/device/instance-placement suites (396 tests), management unit/action/eviction suites (34 tests), CLI standalone lifecycle/start suites (32 tests), and all-target clippy for the store and management crates.

# Release note: Fixes

- A deployment left queued after a memory-admission refusal can start after its configuration is replaced with one that fits. Obsolete queued starts are cleaned up without discarding uncertain ownership, and stale lifecycle refusals no longer suggest that the resource policy is unavailable.
