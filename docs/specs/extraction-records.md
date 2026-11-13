# Extraction records

**Status: proposed, follow-up release only. Not part of 0.2.0 or the initial
`ShardioAI/extract-4b` release.** ADR 0041's nine field types and wire contract remain
unchanged. This note proposes a tenth type; it does not authorize implementation,
training, additional hardware use or changes to current readiness checks.

## Goal

Extract a table as rows of typed columns in one model pass, preserving which cells
belong together. A flat list of names and a separate list of prices cannot express
that association reliably. Target examples are receipt line items, invoice rows,
rosters and repeated prose records. A `records` value is not an unstructured JSON
escape hatch and is not an alias for the current `list` type.

## Proposed contract

Keep `api: extractions` and `POST /v1/systemone`. A field with `type: records`
declares a closed map of named columns; each column has a description and a typed
schema. Initially consider text, number, date, time, datetime and duration columns,
reusing their scalar settings and answers. Nested records, lists, estimates and
forecasts in cells are not proposed for the first records release. The permitted
column set and settings must be settled before the contract is accepted.

Illustrative request field (proposed syntax, not accepted by current servers):

```json
{
  "type": "records",
  "instructions": "Purchased line items in receipt order",
  "columns": {
    "item": {"type": "text", "instructions": "Product name"},
    "quantity": {"type": "number", "instructions": "Quantity purchased"},
    "price": {"type": "number", "instructions": "Extended line price"}
  }
}
```

The proposed answer contains `type: records` and `value`, either null or an ordered
array of rows. Every row is an object with exactly the declared column names; each
cell carries the existing answer for its scalar type, including its null decision,
guess and confidence, and text spans where applicable. Do not synthesize a string
for a missing cell or drop the row because one cell is unknown. An explicitly
empty table is `[]`; null means the input does not determine the table. The exact
field-level uncertainty members, whether rows need their own confidence, and how a
row with no determined cells is represented remain open below.

Row order follows the source, not model confidence. Each row is a distinct
occurrence: identical repeated rows must not be deduplicated. Text spans retain
ADR 0041's offsets into the state string. Image-only text transcription would
require a separate decision; this proposal does not silently expand the initial
model's text-copy contract. Arithmetic cells use operand evidence and the existing
exact compute executor, scoped to the selected row.

The router must validate the new field schema before activation: a nonempty column
map, supported column types, closed settings and any accepted size bounds. The
engine remains responsible for settings' meaning and computed values. Final
syntax, bounds and failure codes require an ADR amendment, examples and tests;
this note is not a schema that clients should send today.

## CapyCTL contract and readiness implications

- Add `records` to the canonical `field_types` declaration and discovery only when
  an engine implements and qualifies it. Existing manifests and fingerprints must
  retain their current meaning. Older binaries reject unknown declarations rather
  than reinterpret records as a list.
- Decide how host capability negotiation distinguishes records support: an old
  `extractions` host must not receive an unsupported manifest or probe. Either a
  records-specific capability or a versioned extraction contract needs an explicit
  compatibility decision, including downgrade instructions.
- Extend request validation, the custom manifest, configuration, protocol evidence,
  readiness, canary storage and comparisons, discovery, fake engine and guides in
  the same release. Do not infer support from an HTTP health response.
- Add one fixed records field to the probe only for a manifest declaring `records`.
  Use a small table with at least two distinct rows, a typed computed cell, a
  missing cell and repeated content that could be associated with the wrong row.
  Validate cell shapes, declared keys and ordering, not semantic accuracy at
  readiness time, as with existing extraction probes.
- Scope saved references by probe version and declared types. Wake comparison
  should require identical row count, order, cell nullness where applicable and
  discrete values/spans, using ADR 0041's inclusive numeric and probability
  tolerances. The final null/guess comparison must follow the accepted cell
  contract rather than invent a different scalar rule.
- Preserve references across the eventual forward-only migration and keep old
  probe versions explicit. Restart-only operation still records a fresh reference
  at each launch; records does not enable parking or LMCache.

CPU tests prove these boundaries, not model quality or native engine qualification.

## Model approach

Reuse the frozen backbone, schema-first layout and attention LoRA. Predict row
count, then perform occurrence-conditioned column readout: row/occurrence queries
identify each row, and each column's schema-conditioned head reads that occurrence.
Reuse span heads for copied cells and operand/operation/unit heads plus the exact
Decimal/calendar executor for computed cells. Condition pointers on the row so a
price cannot be paired with the next product merely because both are plausible.

A count and readout design needs an explicit maximum row count and behavior on
overflow; neither truncation nor returning a confident partial table is an
acceptable implicit policy. Train count, row association, cell presence, span and
compute targets jointly. Evaluate row ordering and duplicate occurrences directly.
This is a proposed approach, not a measured successor to the v7 list head. It must
show improvement on held-out records before replacing or extending released heads.

## Data and evaluation

Generate tables and repeated prose from typed source records, retaining exact row
identity, order, cell spans and computation operands. Vary row counts, column names
and descriptions, column order, irrelevant totals, missing cells, duplicate rows,
units, dates, qualifiers and interleaved distractor records. Render receipt/table
images with the same provenance; image evidence must obey the accepted copy
contract. Use only commercially permitted external datasets, checking source
licenses before including them.

Split by templates, source documents and entities, with held-out combinations of
schemas and computations. Include empty/absent tables, count overflow, repeated
values, cross-row arithmetic traps and long tables. Report text and image subsets
separately, and evaluate current scalar/list tasks to detect regressions.

Required metrics include whole-table exact match, row count accuracy, ordered row
exact match, cell exact match by type, row-association precision/recall/F1, text
span exact match, computed-value accuracy, missing-cell/null accuracy, and
confidence calibration. Report matching rules explicitly: unordered row matching
can diagnose cell quality but must not hide incorrect source order. Stratify by
row count and table length; measure inference latency and memory as readout scales.
Acceptance thresholds and a live qualification matrix must be agreed before the
follow-up ships; pilot scalar/list scores are not evidence for records readiness.

## Open questions

1. What row and column bounds fit the serving memory/latency envelope, and what
   explicit outcome represents overflow without silently truncating?
2. What exact response members distinguish absent tables, empty tables, uncertain
   rows and null cells, and how are their confidences calibrated?
3. Is source order always the public order, including multi-page tables and records
   assembled from prose? How are stable row identities represented, if at all?
4. Which scalar column types/settings ship first? Are computed operands restricted
   to one row, or may a declared cell deliberately reference totals or another row?
5. What image evidence and OCR policy can extend text copying without fabricating
   state-string offsets?
6. Does negotiation use a new capability or a versioned extraction contract, and
   what manifest/probe migrations make mixed-version hosts fail closed?
7. Does occurrence-conditioned readout learn duplicate and long-table association
   reliably enough, and which losses and capacity are needed beyond a row count?
