# Status: Preserve structured SGLang access logs — 2026-11-10 (branch `fix/sglang-log-records`)

SPEC §13.3 / T21 requires credential redaction without breaking engine logging. The startup guard now redacts formatted log output instead of replacing each record's message and arguments. Uvicorn-style access formatters retain their five arguments, including numeric status codes, and handlers created before the guard remain protected. Redaction covers interpolated bearer values and dictionary arguments under both debug settings.

The new `test_access_log_preserves_structured_args_and_redacts_formatted_output` failed before the fix with `TypeError: cannot unpack non-iterable NoneType object` in both debug modes and passes afterward. All 26 startup-guard tests pass, including existing credential-redaction and spawned-interpreter coverage. CPU and Fake-engine tests are not qualification. A native SGLang access-log recheck is still required on the qualification VM and the authorized lab machine that exhibited the defect.

# Release note: Fixes

- **SGLang access logs remain usable.** Credential redaction no longer destroys the structured arguments required by Uvicorn's access formatter, preventing logging errors on requests while keeping credentials redacted.
