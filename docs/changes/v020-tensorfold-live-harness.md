# Status: Repair TensorFold qualification fixtures and controller cleanup — 2026-11-12 (branch `live/v020-triage-tf`)

ADR 0039 (T41) live-row triage corrects the topology fixture so it reaches native resolution, accepts the specified unsupported park refusal, resets cancellation trials by verified stop, reads native timing fields in their original units, sizes mutually exclusive grants against the published unified managed limit, and checks the Python line's actual `ok` health field. Controller re-proof now waits through management startup, and isolated-role cleanup stops the replacement controller and checks that its listeners have closed. This prevents the TF10 replacement controller from escaping cleanup and interfering with later isolated rows.

The first host A run reached neither TF12's idle-unload validator nor TF13/TF14's engine criteria: their management plane was unavailable after TF10. TF10's failed status and cleanup calls do not establish a runtime adoption or stop defect. The main controller cannot manage a deployment held in a separate isolated state directory. The leftover native process is a serious harness ownership failure; a rerun must prove adoption preserves its process identity, stop proves its exit, and both role listeners close before another row starts. No live access or engine operation was performed during this repair.

Owner-decision amendment of 2026-11-12 aligns ADR 0039 §§6 and 16 and TF4 with the shared product codes: exactly `multi_gpu_unsupported` for a one-host topology and `group_shape_unsupported:tensorfold` for a multi-host shape. The fixture reaches each topology gate without the unrelated short-resource-form rejection. Product code and its existing regression tests are unchanged.

Verification: 112 Python harness tests passed; ShellCheck 0.11.0 passed for the three harness shell scripts; the full catalog dry run passed. CPU and Fake-engine tests are not qualification. All affected native rows still require live reruns on host A.

# Release note: none
