# mllm — working agreement

Rust controller for managing inference-engine deployments on hardware that cannot
keep all model weights resident. The primary audience is home users and prosumers
running models on GPU machines they own.

Current phase: 0.1.0 release candidates on `main` (two-host, single-rank vLLM and
SGLang; TP2 is parked). Releases are published as GitHub pre-releases in this
private repository.

This file is the working agreement for every contributor, human or coding agent.

## Authoritative documents

Read these in order when orienting. Lower entries never override higher ones.

1. `docs/SPEC.md` — authoritative product requirements, MUST/MUST NOT, delivery
   gates (§18) and the T01–T40 acceptance matrix (§20).
2. `docs/design/adr/` — architecture decision records. ADR 0012 onward records
   owner decisions made after the F2 design (deep parking default-on, instances and
   placement, engine configuration, per-instance lifecycle, revoked-host recovery,
   version skew, engine registration, and ADR 0019 for discrete GPUs and the network
   inference endpoint).
3. `docs/design/milestones/f2-sglang-design.md` — approved F2 design.
4. `docs/plans/` — per-slice implementation plans. Feature design notes written
   ahead of their plans live in `docs/specs/`.
5. `docs/runbooks/f2-current-status.md` — **the single status authority.**
   What is done, what remains, what needs owner attention, what is queued next.

If a summary disagrees with `docs/SPEC.md`, the spec wins.

Operator-facing docs: `docs/operations/install.md` (install, services, upgrades,
discrete GPUs), `docs/operations/configuration.md` (every setting) and
`docs/operations/network-access.md` (the inference endpoint on the network).
Live test harness: `scripts/live/matrix/README.md`.

## Documentation rules

These exist because process artifacts once grew to 376 files and 7.4 MB and
became harder to audit than the code they described.

- **One status document.** `docs/runbooks/f2-current-status.md` only. Do not create
  `progress.md`, `continuation-*.md`, or per-slice status narratives. Update the
  runbook in place.
- **Never persist regenerable output.** No `review-*.diff` files — a diff is
  `git diff <base>..<head>`. No saved reviewer/agent prompt files. Record the
  commit range, not the bytes.
- **A per-unit report lives only while its unit is uncommitted.** Once the unit
  commits, its evidence belongs in the commit message and the status runbook;
  move the report to the slice's `archive/` directory.
- **Prose is normal English**, in documents and commit messages, regardless of
  chat style settings.
- Local working notes (per-unit reports, review ledgers) are gitignored, so
  anything written there is unrecoverable once removed. Archive rather than delete.

## Verification

Core suite (the authoritative run used for integration):

```bash
cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management \
  -p harness --all-targets --no-fail-fast --locked -- --test-threads=4
```

Also run `cargo test --workspace --all-targets --locked`. Clippy must pass with
warnings denied across those crates. Formatting must pass: `cargo fmt --all --check`. Installer changes also run
`scripts/test-install.sh`; packaging changes run `scripts/verify-packaging.sh`.

One owned-state test spawns a child process that reports a nested summary; the
distinct test total excludes that duplicate.

**CPU and Fake-engine tests are not qualification.** Passing tests never establish
that a native engine recipe works. Say so explicitly in any status claim.

There is no CI while GitHub Actions minutes are exhausted: local verification is
the evidence for a pull request. Work on a branch and open a PR against `main`;
never push to `main` directly. Never publish a GitHub release; the owner publishes.

## Hard constraints

- Live work is authorized on the two lab hosts, host A and host B, with the
  control-plane server on the control host. Their real names and addresses live
  only in the untracked `scripts/live/matrix/hosts.local.env` (see
  `hosts.example.env`). Owner authorized host B on 2026-09-19.
- Do not change engine environments, drivers, or reboot hosts. The only owner
  exceptions are the existing SGLang 0.5.20 venv on both hosts and the mirrored
  vLLM 0.29 venv on host B; do not create or modify others.
- Only one live session runs on the hosts at a time.
- Fault injection is limited to signals sent to mllm-owned processes (PIDs taken
  from ownership evidence) and one bounded external memory allocation. Never change
  firewalls or interfaces.
- Never read or print engine keys or other secrets. The server's SQLite ledger is
  read-only for tests and investigation.
- Native entrypoint denials stay closed until their prerequisites are met.
- Deep-park / collective-RPC paths are security-gated (SPEC §9.1, T21, ADR 0012):
  enabled by default, a host opts out, controls never leave loopback, and they are
  not production-safe. The protections stay mandatory whenever deep parking is on:
  loopback-only engine listener, a per-launch engine key, mllm's key-guard
  middleware, and no engine control path through host ingress or the router. vLLM
  development mode remains an isolated integration, not a production-hardened one.

## Code conventions

- Cite the governing requirement inline where behavior is spec-driven, e.g.
  `// SPEC §6.1: liveness of an HTTP server is not model readiness`.
  `crates/mllm-adapters/src/vllm/adapter.rs` is the reference example.
- Tag tests with their acceptance-matrix ID (`// T16`) so §20 coverage is
  mechanically checkable.
- Uncertainty must retain accounting. Never release a reservation, advance an
  epoch, or replay a dispatch without verified evidence.
