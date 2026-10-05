# Live scripts

`matrix/` is the two-host matrix harness ([its README](matrix/README.md)).
`engine-quick-check.sh` is the per-release engine check below.

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
   start of the old version (or `n/a` when both fit), the no-drafts variant,
   engine `/metrics` and `capyctl status` during a request.
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
