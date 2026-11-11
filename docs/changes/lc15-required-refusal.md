# Status: Enrolled-host LMCache refusals complete the pending start — 2026-11-10 (branch `fix/lc15-required-refusal`)

ADR 0033 §9, ADR 0034 LC15 and SPEC §13: the required-LMCache installation
check already ran on the enrolled host, before private-ingress key storage. Its
`sleep_clears_cache` refusal was lost in the controller's `receive_provision`:
that path accepted only ordinary policy refusals, whereas launch-result validation
also accepted the closed LMCache refusal set. The valid refusal was treated as an
authentication failure, closing the control session and leaving the pending
provision observer unresolved. The live `start deployment --wait` reached the
harness's 555.85-second timeout; the server and host logs recorded the session
failure and reconnection attempts.

Private-ingress acknowledgements now accept the same closed pre-effect LMCache
refusal categories as launch results. Free-form reasons remain denied. The
existing remote-initialization path returns the refusal without sending a launch,
and failure settlement preserves `lmcache_unavailable:sleep_clears_cache` on the
failed start operation instead of waiting for its initialization deadline.

Tests (T45 T34; failing before, passing after):
`required_lmcache_provision_refusal_is_prompt_and_keeps_the_session` reproduces the
authentication failure and timeout through real mutual-TLS host sessions. It now
receives both `sleep_clears_cache` and `check_failed` within a five-second bound,
keeps the same authenticated session, and records no host execution. A coordinator
regression verifies prompt start settlement, the exact typed operation error, a
released binding and no retained reservation; the existing checkpoint-mismatch
case remains covered.

Targeted checks passed: controller agent-session integration tests (21), controller
refusal-settlement unit tests (2), protocol LMCache tests (5), and
`cargo fmt --all -- --check`. Cargo ran with `CARGO_TARGET_DIR` unset, this
worktree's own target directory, and the shared engine lock. CPU and scripted-host
tests are not engine qualification. No live rerun was performed; the remaining
live check is LC15's required start returning the typed refusal promptly on the
enrolled host.

# Release note: Fixes

- **Required LMCache failures no longer leave enrolled-host starts waiting.** A
  host's preflight refusal now reaches the pending start as a typed failure,
  without closing its control session or starting the engine.
