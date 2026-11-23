# Status: Extraction input declarations — 2026-11-23 (branch `feat/extraction-inputs`)

The input-kind amendment to ADR 0041 adds optional extraction manifest `inputs`:
a non-empty, distinct subset of `text`, `image`, `pdf`, stored in canonical order
and refused on other APIs. Omission means all three kinds and preserves existing
manifest digests. Explicit declarations are approved, digested and frozen with
the custom profile. Discovery exposes effective `capyctl.inputs`; undeclared
source kinds receive HTTP 400 `input_unsupported` before waiting or activation.
Non-empty text counts as text except a PDF asset key; non-empty images count as
image, and a PDF asset counts as PDF. Empty media needs no capability.

Readiness uses a fixed source of a declared kind, preferring text. Configuration,
unit, router, ingress and readiness tests cover the declaration and its defaults.
The router regression first exposed a 500 for the new refusal; it now proves 400
with no activation or dispatch. `scripts/ci-local.sh` and the final
`scripts/ci-local.sh --deep` passed, including core and full workspace suites.
The deep run used a dedicated target, four build jobs, no debug information and
no incremental compilation after an earlier run exhausted local build storage.
The site check passed, including guide voice validation. CPU and Fake-engine
tests are not qualification. Live qualification of the actual engine installation
and each advertised input kind is still required; readiness does not establish
that qualification or quality.

# Release note: Requests

- **Declare extraction inputs.** Custom extraction manifests can restrict accepted
  sources with `inputs: [text, image, pdf]` or any non-empty subset. Existing
  manifests still allow all three. Model discovery lists the effective input
  kinds, and requests using an undeclared kind receive `input_unsupported`
  before the model starts.
