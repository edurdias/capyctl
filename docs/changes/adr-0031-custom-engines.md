# Status: ADR 0031 accepted: custom engines — 2026-10-20 (branch `docs/adr-0031-command-engines`)

ADR 0031 records the owner's decisions of 2026-10-20 (proposed 2026-10-18) for custom
engines in 0.1.6: a generic engine kind, `custom`, for one-off engines CapyCTL has no
integration for. It is not a custom build of a known engine, and not ADR 0008's runtime
type `command`. A host administrator registers one with `capyctl engine add --custom <exe>
--name <n> --manifest <file>`, confirmed on a terminal or with `--yes`. The manifest is the
whole argument template (placeholders `{host}`, `{port}`, `{model_dir}`, optional
`{served_name}`, `{context_length}`, `{api_key}`), its API (`chat` or `decisions`), an
optional idle signal and an optional version string. The digest covers the executable and
its whole environment by default (site-packages and interpreter), or a narrower root the
manifest names; drift warns unless `installation_drift: refuse`. CapyCTL keeps process
ownership, the loopback port, routing, authentication, admission, readiness through the
existing chat and decision probes, measured peaks and logs. It refuses parking, groups,
derived sizing (`resources` is required), extra arguments, typed engine fields and
parsers. After readiness, a loopback-listener check stops a launch whose processes listen
off loopback (`custom_listener_exposed`). Without an idle signal, a lease left cancelling
by a hang-up, a deadline cut or an engine error settles only at the instance's next
verified stop or restart, drains do not wait for it, its memory is not freed before that
exit, and status says so (a SPEC §10 amendment). The host capability is `custom_engines`;
the acceptance row is T44. `/v1/systemone` stays closed, and basal's extensions need
their own ADR. The worked example is `basal-serve` 2.2.0 `--mode eager` for basal-omni
decision models: no API key, no in-flight counts, and a 422 for every failure, which may
mean an engine failure (upstream issue rkinas/basal#22). Live rows CE1–CE4 will run on
host A with `~/basal-2.2.0-venv` (owner approved 2026-10-20, added to AGENTS.md). Nothing
is implemented; the SPEC amendments it lists land with the implementation.

# Release note: none
