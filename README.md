# mllm

Engine-neutral lifecycle and inference-routing controller: one endpoint, bring-your-own
inference engines, explicit deployment ownership, safe model residency transitions, and
aggregate resource control.

**Status:** design phase — no runtime exists yet.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — authoritative architecture and requirements (rev 0.2).
- [`docs/AGENT_HANDOFF.md`](docs/AGENT_HANDOFF.md) — coding-agent brief.
- [`docs/design/0000-full-picture.md`](docs/design/0000-full-picture.md) — approved
  decisions, crate layout, and milestone decomposition.
- [`docs/design/adr/`](docs/design/adr/) — architecture decision records.
- [`docs/examples/`](docs/examples/) — illustrative YAML sketches (parse-only, not
  calibrated recipes).

License: Apache-2.0 (see [ADR 0006](docs/design/adr/0006-license.md)); `LICENSE` file is
added before publication.