# Status: Clairic-flash extraction integration — 2026-11-23 (branch `docs/clairic-flash-extractions`)

Documented the `clairic-extract` custom-engine implementation in extraction
server branch `feat/clairic-flash-serving`: native Cloudflare
`load_release_model`/`systemone`, `/health`, `/v1/models`, `/v1/systemone`,
per-launch authentication, all nine ADR 0041 field types and text/image inputs.
The local executable snapshot is pinned to
`36ad68b999560f56b57ef16a22180f8cb83c6d77` and is Apache-2.0; its third-party
notices stay with the snapshot. No upstream code or weights are redistributed
here. This documentation makes no change to Rust or the request/lease contract.

Generated scalar candidates are adapted to CapyCTL's guess/value shape while
retaining native null probabilities, completeness flags, real spans and generated
token usage. Native nulls never become fabricated guesses. The wrapper keeps
concurrency one, defaults to 8,192 tokens and explicitly selects portable SDPA.
The initial 32-GiB GPU/8-GiB RAM reservation is unmeasured, not a qualified
hardware requirement. The recipes repository remains measured-configuration
only, so candidate configuration lives beside the wrapper for now.

Ordinary image responses with null spans meet the existing whole typed-envelope
completion rule. Native Clairic PDFs render visually, however, and can return
non-null text with null spans and no OCR `state_text`. ADR 0041's existing PDF
validator refuses that evidence. Per the integration owner's explicit scope
decision, the wrapper currently rejects PDF assets before inference rather
than weakening validation or claiming text-only PDF support. Proposed
[ADR 0042](../design/adr/0042-visual-pdf-extraction-evidence.md) records the owner
decision needed: default-strict manifest-declared visual PDF evidence permitting
explicit null spans, never fabricated offsets or source text. The proposal is
not approved or implemented. This restriction is independent of our own model's
text-only release decision. Clairic's additional native field types remain
outside the closed CapyCTL API.

The wrapper's new fake-backend tests first failed with the expected missing
module, then the focused wrapper/existing-contract suite passed **69 CPU tests**.
That evidence belongs to the server branch, not to a Rust integration run.
CPU and fake-engine tests are not qualification. The server's serving guide has
exact deferred commands using the approved session wrapper,
`--disable-shardio-lifecycle`, unchanged owner/watcher guard, finite deadline,
private journals and verified cleanup. Live qualification still requires the
exclusive evaluator's host turn, available approved native dependencies, pinned
host-local weights, actual nine-type CapyCTL readiness, image checks, measured
memory, restart and stop receipts. No live run, package install, new environment,
PDF capability or published model is claimed.

The CapyCTL documentation branch also passed the complete fast
`scripts/ci-local.sh` gate under four CPU cores and 16 GiB: formatting, naming,
workspace clippy, unit tests, 398 runtime Python tests, pinned shellcheck,
installer tests and change-assembly tests. No Rust files changed. The serving
guide's three shell command blocks passed `bash -n`, and its embedded deferred
live-check Python parsed without executing any live command.

# Release note: Models

- **Clairic-flash setup.** The extraction deployment guide now points to the
  native-runtime custom-engine wrapper and candidate text/image manifest,
  including context, concurrency, memory and Apache-2.0 notices. The integration
  is not yet live-qualified. Visual PDF evidence is an owner-decision proposal;
  PDF assets are not yet supported by this wrapper.
