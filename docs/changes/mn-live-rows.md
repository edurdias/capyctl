# Status: Multi-node live rows MN1-MN8 pass on the 0.1.3 build — 2026-10-15; SGLang groups are restart_only (branch `test/live-rows-0.1.3`)

Plan `docs/plans/2026-10-05-multi-node-groups.md` Task 21, design
`docs/specs/2026-10-05-multi-node-groups-design.md` §17.2, ADR 0028. The live
harness gains the multi-node group rows, first run live 2026-10-15 (below).

- `scripts/live/matrix/rows/MN1.sh` … `MN8.sh`: bring-up at TP 2 for vLLM,
  SGLang and TensorFold (Ready, 20/20 routed completions, RDMA counters before
  and after on both hosts recorded as `transport: rdma` or `socket`), five deep
  park and wake cycles with per-member reservations, MemAvailable on each host,
  the wake canary's reference tokens and one `parked`/`restored` journal entry
  per cycle (MN4 vLLM; MN4 SGLang checks the restart_only park and the refusal of
  `residency: deep` instead), SIGKILL of the worker's or the head's engine from its
  recorded member identity (MN5, MN6), host B's agent stopped and restarted
  through the harness's role control while Ready (MN7: `uncertain` with the
  charge held, then settled), and the group alternating with a single-rank
  deployment on host A under `start --evict` (MN8). Every row starts with a
  read-only preflight (both hosts online with `engine_groups`, each host's
  direct-link address as its peer address, one build fingerprint for the
  engine's profile on both hosts, the checkpoint on both with the same files)
  and cleans up on failure through the row trap; MN7's trap restarts host B's
  agent first.
- `grouplib.sh` (group status and ledger views, cleanup on both hosts),
  `gen_group.py` (fixtures `vg-30`, `sg-30`, `tg-n35`), `rdma_counters.py`
  with `tests/test_rdma_counters.py`, `ledger.py group` and `members`
  (read-only views of `group_plans`, `group_members` and member charges), and
  `fault.sh member`.
- `roles.sh host-doc` declares `resource_policy.groups` (peer address
  `HOST_<A|B>_DIRECT_ADDR`, rendezvous range `GROUP_RENDEZVOUS_PORTS`,
  default 25000-25099; `CAPYCTL_GROUPS=0` leaves it out) and, with
  `TENSORFOLD_VENV_DIR`, a `tensorfold` profile written as `engine add`
  writes one. The harness's engine process pattern now covers
  `bin/tensorfold serve`.

Checked: `bash -n` and `shellcheck -x` (v0.11.0) on the rows and changed
scripts, a dry run of every row, `python3 -m unittest discover -s
scripts/live/matrix/tests`, `test_bench.py`, and the three fixtures resolved
by `capyctl validate config --host … --host …` against generated host
documents. These are harness checks, not evidence: CPU and Fake-engine tests
are not qualification; the rows MN1-MN8 on the two hosts are.

Final rerun, third session, on the qualifying build (the second session's build plus
three fixes: departed processes are never recorded, TensorFold probe ids from
`tensorfold.token_ids`, SGLang groups restart_only). Fresh run, every row passed:

- MN2 SGLang: Ready 225 s, 20/20, rdma.
- MN3 TensorFold: Ready 111 s on a cold kernel build (13 s warm), 20/20, rdma. Real
  canary token ids, and no text-evidence notice in the controller log.
  `TF_NCCL_LIB` on both ranks is the venv's
  `…/site-packages/nvidia/nccl/lib/libnccl.so.2`, read from each member's recorded
  process.
- MN5 SGLang and TensorFold, MN6 TensorFold: the Ready plan settles in 2–4 s at the
  killed rank, the relaunch is Ready (SGLang 223 s, TensorFold 12 s), answers, and
  stops clean. SGLang's transient child lands in the host journal's
  `departed_processes`, not the record.
- MN4 SGLang, restart_only: status reads `restart_only`; a park stops the group in
  4 s with nothing left on either host; a request brings it back on plan 2 (217 s).
  `residency: deep` on the same fixture is refused at deploy
  `capability_missing:deep_park`, and the message names `restart_only`.
- MN1 vLLM sanity: Ready 110 s, 20/20, rdma.

MN4 vLLM, MN5/MN6 vLLM, MN6 SGLang, MN7 and MN8 pass from the second session (below)
and were not rerun.

Second session, 2026-10-15, on the 0.1.3 build before those three fixes: origin/main
with #124 and the
group fixes (TF_NCCL_LIB, unreachable member, orphan worker, deep park, stable role
names, unrecorded-member settlement). Evidence is under `target/live/matrix/MN*/`, the
summary in `target/live/matrix/0.1.3-summary.md`. vLLM 0.30.0 with one build
fingerprint on both hosts, SGLang 0.5.21, TensorFold 0.6.5.

- Pass:
  - MN1 vLLM (Ready 120 s, 20/20) and MN2 SGLang (Ready 252 s, 20/20), both
    `transport: rdma` by RDMA operation counters on `roceP2p1s0f1` and
    `rocep1s0f1` on both hosts, memory back after stop.
  - MN4 vLLM: five deep park and wake cycles. Parked in 10 s, woken in 49–74 s,
    87–90 % of the Ready drop released on each host, each member charged its
    measured residue (about 3.4 GB), the canary's reference tokens equal every
    cycle, 5 `parked` and 5 `restored`.
  - MN5 vLLM and MN6 vLLM/SGLang: the Ready plan settles in 2–7 s with the killed
    rank as `failed_rank`, the group relaunches under `recovery: reconcile`,
    answers, and stops with nothing left on either host.
  - MN7 vLLM: rank 1 `uncertain` and still charged while host B's agent is down,
    then back and serving.
  - MN8 vLLM: two `--evict` alternations.
- Fixed during this session, verified live: a SGLang worker's transient child gave
  the tree two `worker-0` roles, so its identities were never recorded and the
  member stayed `uncertain` with 54 GB charged. After a restart on the same state
  with the fix, it settled within 12 s and the charge was released.
- Product failures (owner decisions 2026-10-15 in brackets):
  - MN4 SGLang: the park completes, but the wake fails. The head reloads its weights
    from disk (186–222 s); within seconds of its reply the restore is
    `restore_uncertain` and the group stops (two runs). [SGLang groups resolve
    `restart_only` and refuse deep for 0.1.3.]
  - MN5 SGLang: the relaunched plan recorded a transient child that had already
    exited, so the member failed right after Ready and the group relaunched again.
    [Being fixed: record only processes alive at report.]
  - TensorFold (MN3, MN5, MN6): both ranks load and serve, but readiness fails
    because TensorFold's `/v1/completions` answers no `token_ids`. [Being fixed:
    text probe.]
- Harness fixes from both sessions:
  - roles run under their own tmux server (an older one passed memlock 8 MiB, and
    NCCL failed);
  - configurable loopback ports for the run server;
  - MN5/MN6 judge the Ready plan and accept the reconcile relaunch;
  - TensorFold group members declare 40 GiB GPU (TensorFold's TP 2 startup estimate
    is 32.49 GiB, so 30 GiB + 2 GiB refused every window);
  - `fault.sh member` picks a group worker's spawned process now that its whole
    tree is recorded;
  - the SGLang group fixture declares no residency, and MN4 SGLang is the
    restart_only check; MN1-MN3 record each member's collective environment
    (`TF_NCCL_LIB`, …) with `proc_probe.py`.
- First session (an earlier build): TensorFold failed on libnccl, MN4 stayed
  `parking`, MN5 vLLM left an orphan `VLLM::Worker_TP1` and stayed charged, and MN7
  never showed `uncertain`. The group fixes above address all four.
- Deviation from the fault rule (first session, MN5 vLLM, 2026-10-15 about 10:30Z):
  - What was sent: to free host B after the stuck member, `signal_owned.py` sent TERM
    and then KILL to `VLLM::Worker_TP1` (pid 3037932), and TERM to its resource
    tracker (pid 3037931).
  - Why it was a deviation: neither pid was in the ledger. Ownership was inferred from
    their recorded parent (the host role's pid file) and matched by pid, start ticks
    and boot id. AGENTS.md allows signals only to ledger-recorded identities, so this
    should not have been done.
  - Evidence: `MN5-vllm.prev-20261015T195859Z/recovery-orphan-term.txt`.
  - In the second and third sessions no process outside the ledger was signalled;
    a row that would need it records the process and stops instead.

# Release note: none
