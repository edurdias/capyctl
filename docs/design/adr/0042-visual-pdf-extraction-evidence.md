# ADR 0042 — Visual PDF extraction evidence

**Status:** Proposed, 2026-11-23. Requires owner approval. Not implemented.
**Would amend:** ADR 0041 §3 (PDF completion evidence), ADR 0031 §2 (an
extraction manifest's optional PDF evidence declaration).

## Context

Cloudflare's Apache-2.0
[Clairic-flash release](https://huggingface.co/Cloudflare/clairic-flash/tree/36ad68b999560f56b57ef16a22180f8cb83c6d77)
supports text, images and PDFs. Its native PDF path renders pages as images;
`serialize_response` returns the request's text state, not OCR text. A visual
text value may be non-null with `span: null`, and a text list may have null
members in its parallel `spans` array. Neither condition implies model failure.

ADR 0041's PDF amendment was designed for text-layer conversion: `state_text`
is mandatory, each reported text span must match its candidate, and the asset
key is never source text. The current `pdf_answered` validator refuses the
native visual-only case. Ordinary image responses are not subject to that
PDF-specific span check; their complete typed envelope already passes ordinary
extraction completion evidence. Startup readiness is a separate fixed text
probe and must stay unchanged.

The Clairic wrapper therefore currently advertises text and images only and
rejects PDF assets before inference. That restriction is not our own model's
text-only release decision. Adding support must not manufacture OCR text,
character offsets or page provenance to make the old validator pass.

## Proposal for owner decision

1. Add optional `pdf_evidence: text | visual` to extraction custom-engine
   manifests. Omission means `text`, retaining existing validation and serialized
   identities. Refuse the setting on other APIs. Freeze it with the manifest so
   it participates in profile/revision identity and is shown at registration.
2. `text` retains the exact existing PDF evidence rules. `visual` additionally
   permits an **explicit null** scalar text `span`, or explicit null entries in a
   text list's parallel `spans`. Missing span members remain malformed. Preserve
   item order, duplicates, array lengths and the distinction between a null list
   and a list of values with unavailable text evidence.
3. `state_text` remains mandatory and may be empty for visual-only input. It is
   the actual retained text, never the asset key or concatenated generated
   answers. Every non-null offset still must be in range and match the returned
   text in Unicode code points. Mixed evidence cannot use the declaration to
   launder incorrect non-null spans.
4. Optional `page`/`pages` remain positive one-based values or null and retain
   their existing shape checks. The wrapper omits them unless the backend knows
   the page; no inferred page numbers are permitted.
5. Keep the same one-PDF request shape, body bound, forwarding, completion/lease
   accounting and restart-only lifecycle. The wrapper may translate the asset to
   native `documents`, remove the state asset key, force return of retained
   `state_text`, and select the native four-page/4-MiB PDF limits. Invalid,
   encrypted or over-limit documents remain complete engine rejections, never
   fabricated successful evidence.
6. Do not broaden startup readiness, wake matching, the nine field types, our
   own model's text-only release, or general remote media/asset support.

This explicitly separates unavailable character evidence from incorrect
character evidence. It is not a claim that model output is factually grounded:
CapyCTL completion evidence establishes a completed typed response, not truth.

## Approval and implementation gates

The owner must approve both the manifest distinction and the weaker evidence
requirement for declared visual PDF engines before code changes. Implementation
would migrate every routing/host ingress completion call to the frozen
capability, test default strictness, explicit nulls, malformed/missing spans,
wrong non-null offsets, Unicode, list lengths, page metadata, registration and
manifest identities. It must retain uncertain leases for malformed responses.
Run CPU checks including `scripts/ci-local.sh`, then qualify actual Clairic text,
image and PDF paths under the approved exclusive host guard, including a scanned
PDF and verified cleanup. Until those gates pass, no PDF capability is advertised
and no native PDF request is forwarded by the Clairic wrapper.
