# Status: Live rows: the harness files #126 left out — 2026-10-15 (branch `test/live-rows-harness-followup`)

PR #126 (MN1–MN8 and LC1–LC6) merged without eight of the harness files the rows ran with, which
stayed uncommitted when the change was ported to a clean branch. This adds them exactly as they
ran in the three live sessions (identical to `test/live-rows-0.1.3`):

- `lib.sh`: the run server's loopback ports (`CAPYCTL_SERVER_MGMT_PORT` and
  `CAPYCTL_SERVER_INFER_PORT`, for a control host already serving 7443/8443), and the harness's
  own tmux server (`tmux -L capyctl-mx`; an old tmux server passed an 8 MiB memlock to the host
  roles, which broke NCCL at group bring-up).
- `roles.sh` and `gen_host_doc.py`: each host document declares `resource_policy.groups` (its
  direct-link peer address and rendezvous ports) and a TensorFold profile when
  `TENSORFOLD_VENV_DIR` is set.
- `ledger.py`: the read-only `group` and `members` views.
- `fault.sh member`: signals a group member's spawned process by its recorded identity.
- `hosts.example.env`, `README.md`, `rows/ENG2.sh`: the new variables, the MN and LC rows,
  and the `local` profile name.

Harness only; no product change. Checks: `bash -n`, `scripts/ci-local.sh --only shellcheck`,
`python3 -m unittest discover -s scripts/live/matrix/tests`.

# Release note: none
