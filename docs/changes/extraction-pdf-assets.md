# Status: Text-layer PDF extraction assets (ADR 0041 amendment) — 2026-11-12 (branch `feat/extraction-models-0.2.0`)

ADR 0041 §3 now accepts an OpenRouter-aligned named `assets` map restricted
to exactly one PDF. Asset members are closed; standard base64 is checked
without decoding. State may be absent, empty or the sole asset key. Other
text, multiple assets, unknown formats or members, and non-empty images
combined with a PDF are refused before activation. Existing image validation
is unchanged. CapyCTL forwards the payload without PDF conversion.

The engine performs column-aware text-layer conversion with aligned ruled-table
rows. Scanned/no-text, encrypted, malformed and over-limit PDFs are engine
`unsupported_input` rejections. Engine defaults are configurable decoded limits
of 4 MiB and 32 pages; the HTTP envelope remains 8 MiB. PDF responses must return
`state_text` regardless of `return_state_text`; text spans index its Unicode
code points, never the original asset key. Optional `page` and parallel `pages`
carry one-based page numbers. Malformed PDF response evidence remains uncertain.
The text-only readiness probe, checkpoints and default recipes are unchanged.

T50 unit coverage adds PDF-only and asset-key acceptance, closed-shape and
base64 rejection, mixed-input rejection, mandatory returned text, Unicode span
matching and optional page metadata. Numeric and normalized date lists check span
bounds without requiring their values to equal source literals. A forwarding
test covers byte-preserved PDF payloads and mandatory returned-text evidence.
The integrating CPU run passed every default fast `scripts/ci-local.sh` step:
format, naming, Clippy, workspace unit/bin tests, runtime checks, ShellCheck,
installer tests and change-file assembly. CPU and fake-engine tests are not
qualification; live PDF extraction quality and engine interoperability still
require qualification.

# Release note: Requests

- **Text-layer PDF extraction.** Extraction requests can supply one named PDF
  in `assets`. The engine returns converted `state_text` and spans into it,
  with optional page metadata. Images and PDFs cannot be combined. The request
  envelope remains 8 MiB; PDF decoding and configurable document limits belong
  to the extraction engine.
