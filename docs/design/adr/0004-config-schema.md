# ADR 0004 — Configuration surface: versioned strict YAML

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §19 lists "YAML/API surface: versioned strict configuration" as a
baseline proposal. The illustrative field names in SPEC §16 and `docs/examples/` are
sketches, not a frozen schema.

## Decision

Server, host, deployment, and standalone configurations are versioned strict YAML files
(`schema_version` required), validated against the boundaries in SPEC §15. Validation
rejects unknown mllm fields, duplicate keys, invalid units, unsatisfied required fields,
unsupported role/adapter combinations, conflicting reserved arguments, invalid cache
references, and contradictory standalone/remote connections.

## Rationale

- Operators author host policy and deployment manifests as files; strictness prevents
  silent misconfiguration (e.g., a YAML value silently treated as a hard GPU cap —
  SPEC §7.5).
- Versioning allows defaults to evolve without mutating pinned deployment contracts
  (T39: existing deployments retain their effective contract until explicit update).

## Consequences

- F0 turns the illustrative fields into a tested schema with fixtures, documenting any
  renaming (SPEC §19 required action).
- No-config startup follows SPEC §15.2 exactly: implicit config may be generated safely;
  explicit missing/invalid config fails; generation is atomic and owner-protected.
- Runtime role flags override ordinary settings under documented precedence, never host
  safety limits or identity checks.
- No arbitrary YAML object construction or shell evaluation.