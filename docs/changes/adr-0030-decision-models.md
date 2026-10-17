# Status: ADR 0030 proposed: decision models — 2026-10-17 (branch `docs/adr-0030-decision-models`)

ADR 0030 records the owner's decisions of 2026-10-17 for decision models in 0.1.5: a deployment
field `api: decisions`, the inference route `POST /v1/systemone`, engines (llama.cpp v0.7.0 and
SGLang 0.5.22 first; vLLM once a release ships the endpoint; TensorFold 1.0 as its own change),
and recipes for Clef-Flash, Clef, Kev-4B and one of Julia-1 or Laya. It proposes readiness by
one fixed, well-formed decision and a wake check that compares probabilities within 0.05, and
three scope limits for the first version awaiting the owner's confirmation (one machine,
restart instead of park, no readout in CapyCTL). Proposed; nothing is implemented. The SPEC
amendments it lists land with the implementation.

# Release note: none
