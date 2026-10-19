# Status: ADR 0031 proposed: command engines — 2026-10-18 (branch `docs/adr-0031-command-engines`)

ADR 0031 (status Proposed; the owner approved writing it on 2026-10-18) designs a generic
engine kind, `command`, for one-off engines CapyCTL has no integration for. A host
administrator registers one with `capyctl engine add --command <exe> --name <n> --manifest
<file>`: the manifest is the whole argument template (placeholders `{host}`, `{port}`,
`{model_dir}`, optional `{served_name}`, `{context_length}`, `{api_key}`), its API (`chat`
or `decisions`), an optional idle signal and an optional version string. The executable
and its environment tree are pinned by digest at registration, with ADR 0008 drift
handling. CapyCTL keeps process ownership, the loopback port, routing, authentication,
admission, readiness through the existing chat and decision probes, measured peaks and
logs; it refuses parking, groups, derived sizing (`resources` is required), extra
arguments, typed engine fields and parsers. Without an idle signal a cancelling request
lease settles only at the instance's next stop or restart, drains no longer wait for it,
and status says so. Other points: a host capability `command_engines`, a loopback-listener
check after readiness, confirmation at `engine add`, refusal codes, and acceptance row
T44. The worked example is `basal-serve --mode eager` for basal-omni decision models
(basal v2.1.0: no API key, no running or queued count, so no idle signal). Ten open
questions for the owner close the ADR, among them the kind's name, which collides with
ADR 0008's runtime type `command`. Nothing is implemented; the SPEC amendments it lists
land with the implementation.

# Release note: none
