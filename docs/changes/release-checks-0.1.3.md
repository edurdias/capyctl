# Status: 0.1.3 release live checks driver — 2026-10-14 (branch `test/backlog-0.1.3`)

A driver for the live checks the changes merged since 0.1.2 still owe (B1-B17
of the 0.1.3 live backlog), on one GB10 host: `scripts/live/release-checks/run.sh`,
with `remote.sh` (host phases, run detached) and `rc.py` (requests, reads and
the verdict, standard library only). Modelled on `scripts/live/engine-quick-check.sh`
and the B18 comparison driver: CapyCTL built on the host into
`~/capyctl-0.1.3-live`, the checks run in `~/c13/<run>` (a short path, since
deep parking's observation socket lives under the state directory), results
copied back to `target/live/0.1.3-backlog/<run>/` with host names redacted,
and the host's work directory removed. `--dry-run` prints the whole plan,
every deployment file and each check's pass condition; `--only` runs a subset.

Each group of checks runs on its own standalone role with its own state,
config and model-sources directories and port (18643, management 18644, engines
8340-8359), each profile with empty kernel caches of its own; the main role
raises `managed_limit` to 100GiB with a 10GiB `free_reserve` and a 45GiB
`parked_limit`. Deployments are shared to keep cold starts down: vLLM 0.30.0
model 1 with no declared loader (B1, B2's three GB10 wakes, B3, B5, B14), its
eager variant (B4), TensorFold 0.6.6 model 1 (B3 with the CUDA `prefill_ms`
mapping, B14's refusal), SGLang 0.5.21 Qwen3-4B (B3, B5, B6, B14; B8 with
explicit activation), SGLang model 1 (B9's two small-KV launches, B15, B12 with
`model.draft`), Qwen2.5-0.5B downloaded by CapyCTL (B11, and the tar payloads
B13 serves from a loopback `python3 -m http.server` to a role that approves
plain HTTP for that origin only). B7 has its own role (110GiB managed, 11GiB
reserve). Every field the checks assert was read from the code: the effective
configuration (`inspect deployment --effective-config`), status
(`parked.growth[]`, `checkpoint_digest.provenance`, `model_sources[]`,
`activation`), `list hosts` (`session.memory_warnings`), the load read, the
engine-log tail route, the role log's `router_selection` events, the
`engine build limits: MAX_JOBS=` line, and the ledger's `wake_canaries` and
`journal_entries` (read-only SELECTs). Engine keys are never read: the log
redaction check is by rule, by the role's own key and token, and by a unique
prompt marker. Every role, deployment, profile, HTTP server and container is
removed at the end, after a failure too.

SKIP by default: B10 (the host's Flash-Next copy is not a complete checkpoint;
`--download-flash-next` lets CapyCTL download it into `~/models/sources`), B16
(no gpt-oss-120b on a host) and B17 (`--container` builds an image on the host).
B18 has its own driver in capyctl-recipes.

Checked: `bash -n`, `shellcheck -x` (v0.11.0) clean, the dry run. Not yet run
on a lab host; the live results go in the status runbook when they exist.

# Release note: none
