# Live scripts

`matrix/` is the two-host matrix harness ([its README](matrix/README.md)).
`engine-quick-check.sh` is the per-release engine check below, and
`release-checks/run.sh` the live checks a CapyCTL release owes.
`multi-gpu/run.sh` runs the one-host multi-GPU rows (MG1–MG14) and a download
bandwidth test on a rented two-GPU machine in one session
([its README](multi-gpu/README.md)).

## Engine quick check

When an engine ships a release, one command checks it against the previous one on
a GB10 host through CapyCTL and writes a short verdict:

```bash
scripts/live/engine-quick-check.sh --engine tensorfold --new 0.6.6 --old 0.6.5 --host a
scripts/live/engine-quick-check.sh --engine vllm --new 0.31.0 --old 0.30.0 --model qwen3-4b
```

It needs `scripts/live/matrix/hosts.local.env` (see the matrix README) and a
checkout of capyctl-recipes beside this repository (`--recipes`, `--bench` or
`CAPYCTL_RECIPES_DIR` / `CAPYCTL_BENCH_DIR` name another one). Output never names
the host. In order, it:

1. Builds CapyCTL from this checkout on the host (`matrix/sync.sh snapshot`,
   `push`, `build`; the tree goes to `~/capyctl-quick-check` there, beside the
   matrix tree). `--skip-build` uses the binary already built.
2. Creates `~/<engine>-<version>-venv` on the host for each version that does not
   have one, with the steps of
   [Install an engine](../../docs/guide/install-engines.md) (TensorFold from its git
   tag with torch 2.13.0 and triton from the cu130 index, vLLM pinned with
   `--torch-backend=cu130`, SGLang with `--prerelease=allow`). An existing venv is
   kept as it is; `--skip-venv` fails instead of creating one.
3. Starts its own standalone role (own state and config directories under
   `~/capyctl-quick-check-runs/<run>`, port 18443, `--port` changes it), adds both
   versions with `engine add`, and checks the lifecycle on the new version: cold
   and warm start, engine `/health`, one streamed request with reasoning and
   usage, stop, a request after stop (409), wake on request after a `--evict`
   start of the old version (or `n/a` when both fit), the no-drafts variant, and,
   during a request, `capyctl status` and CapyCTL's load read
   (`/management/v1/metrics/load`), which must carry fresh engine samples. The
   engine's own `/metrics` is read with the launch key, which the harness never
   reads; under deep parking vLLM keys it (CapyCTL's guard), so an unkeyed
   read is only noted (`metrics-notes.txt`).
4. Benchmarks old against new with capyctl-bench: C1, C4 and C8 at 512 tokens,
   5 rounds after a warm-up, in 2 interleaved passes (old, new, old, new), then a
   context sweep (2k, 32k, 128k, 3 runs, one pass). The passes are pooled and the
   greedy outputs compared within and between versions (TensorFold's `token_sha`;
   vLLM and SGLang report no token hash, so only reply lengths are compared).
5. Writes, under `target/quick-check/<date>-<engine>-<old>-vs-<new>/`
   (`--out`, or `CAPYCTL_QC_OUT_ROOT` for the parent directory): `verdict.md`
   (PASS, CHECK or FAIL; lifecycle table, speed table with points more than 5%
   worse marked, `--threshold` changes it; output match; anomalies), `versions.md`,
   `report/` (`capyctl-bench report --summary`), `results/` and `raw/`.
6. Cleans the host: deletes the deployments, removes both profiles, stops its
   role and removes its work directory. Venvs and models stay.

Venv retention: per engine, the two newest venvs on the host, the versions named in
capyctl-recipes and this run's two are kept. The others are printed as removal
candidates and removed only with `--prune`.

Model presets (`--model`, settings from the capyctl-recipes GB10 recipes):

| Preset | Engines | Notes |
|---|---|---|
| `qwen38-27b` (default) | tensorfold, vllm, sglang | Qwen3.8-27B NVFP4 with the DFlash2 drafter in `~/drafters/Qwen3.8-27B-DFlash2`; context sweep on a 262144-token deployment |
| `nemotron` | tensorfold | Nemotron 3.5 Lightning, built-in MTP, thinking on; C1 only, no context sweep |
| `qwen3-4b` | vllm, sglang | Qwen3-4B, no drafter, no no-drafts variant; sweep 2k, 8k, 32k on the model's own window |

A full run with the 27B model takes about an hour; `--rounds 2 --runs 1` about half.
The host phases run detached from the SSH session, so a dropped connection does
not stop them. If the script itself is interrupted, run
`bash ~/capyctl-quick-check-runs/<run>/remote.sh down` on the host to stop the role
and delete what it created.

## Release checks

`release-checks/run.sh` runs the live checks a release owes (for 0.1.3: B1-B17
of the live backlog, from the `docs/changes/` files merged since 0.1.2) on one
GB10 host, and writes `verdict.md` with one PASS, FAIL or SKIP row per check
and the evidence of every sub-check:

```bash
scripts/live/release-checks/run.sh --dry-run          # the whole plan and per-check estimates; no host contacted
scripts/live/release-checks/run.sh                    # every check (about 2.5 hours on host A)
scripts/live/release-checks/run.sh --only B4,B7       # a subset; B2 brings B1, whose cycles it rides on
```

It builds CapyCTL on the host like the quick check (tree `~/capyctl-0.1.3-live`)
and runs the checks detached in `~/c13/<run>` there, a short path on purpose:
deep parking's observation socket lives under the state directory. Each group
of checks gets its own standalone role, state directory, model-sources
directory and port (18643 by default, `--port`), and each profile its own
empty kernel caches. One deployment serves every check it can: vLLM model 1
with CapyCTL's default loader for B1, B2, B3 and B5, then a stated lazy loader
(an informational B1 row that never decides B1); vLLM Qwen3-4B for B14; the
eager loader on its own role with `parked_growth_limit: 0B` for B4 (SKIP when
the parked charge never grows); TensorFold 0.6.6 with the DFlash2 drafter for
B3 and B14; SGLang Qwen3-4B for B3, B5, B6, B14 and B8; SGLang model 1
(`restart_only`) for B9, B15 and B12; Qwen2.5-0.5B, downloaded by CapyCTL, for
B11 and the ustar payloads of B13. Every role,
deployment and profile is removed at the end, after a failure too; venvs and
models stay. Engine keys are never read, and the ledger only through read-only
SELECTs. B10 (Flash-Next with its table on disk) runs only with a complete
checkpoint on the host or `--download-flash-next`, B17 (a role as a container's
PID 1) only with `--container`; B16 is SKIP until gpt-oss-120b is on a host.
Results go to `target/live/0.1.3-backlog/<run>/` in the main checkout. If the
script is interrupted, `bash ~/c13/<run>/remote.sh down` on the host stops the
role and deletes what it created.
