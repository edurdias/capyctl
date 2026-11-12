# Status: Repair TensorFold qualification fixtures and controller cleanup — 2026-11-12 (branch `live/v020-triage-tf`)

ADR 0039 (T41) live-row triage corrects the topology fixture so it reaches native resolution, accepts the specified unsupported park refusal, resets cancellation trials by verified stop, reads native timing fields in their original units, sizes mutually exclusive grants against the published unified managed limit, and checks the Python line's actual `ok` health field. Controller re-proof now waits through management startup, and isolated-role cleanup stops the replacement controller and checks that its listeners have closed. This prevents the TF10 replacement controller from escaping cleanup and interfering with later isolated rows.

The first host A run reached neither TF12's idle-unload validator nor TF13/TF14's engine criteria: their management plane was unavailable after TF10. TF10's failed status and cleanup calls do not establish a runtime adoption or stop defect. The main controller cannot manage a deployment held in a separate isolated state directory. The leftover native process is a serious harness ownership failure; a rerun must prove adoption preserves its process identity, stop proves its exit, and both role listeners close before another row starts. No live access or engine operation was performed during this repair.

TF4 retains ADR 0039's `group_unsupported` criterion. Existing product regression tests instead specify `multi_gpu_unsupported` for local topology and `group_shape_unsupported:tensorfold` for a group. This intentional product/test contract differs from ADR 0039 §§6 and 16 and needs an owner decision rather than silently broadening the qualification assertion. The repaired fixture exposes that discrepancy instead of accepting an unrelated resource-form rejection.

Verification: 112 Python harness tests passed; ShellCheck 0.11.0 passed for the three harness shell scripts; the full catalog dry run passed. CPU and Fake-engine tests are not qualification. All affected native rows still require live reruns on host A.

# Release note: none
