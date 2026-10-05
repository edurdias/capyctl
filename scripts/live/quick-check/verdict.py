#!/usr/bin/env python3
"""Write verdict.md and versions.md for one engine quick check.

usage: verdict.py OUT_DIR SETTINGS_JSON

OUT_DIR holds raw/ (the host's res directory: lifecycle.tsv, startup.txt,
health-*.log, role.log, versions.txt, ...) and results/ (merge_passes.py output).
SETTINGS_JSON names the run: engine, engine_label, old, new, model_title,
model_ref, drafter_ref, threshold, rounds, passes, runs, concurrency, sweep,
command, capyctl_commit, snapshot, bench_commit.
"""
import json, re, sys
from pathlib import Path

out = Path(sys.argv[1])
cfg = json.loads(Path(sys.argv[2]).read_text())
raw, res = out / "raw", out / "results"
old, new, label = cfg["old"], cfg["new"], cfg["engine_label"]
thr = float(cfg["threshold"])


def load(v):
    p = res / f"{cfg['engine']}-{v}.json"
    return json.loads(p.read_text()) if p.exists() else {}


R = {"old": load(old), "new": load(new)}
match_report = json.loads((res / "match.json").read_text()) if (res / "match.json").exists() else {}
match = match_report.get("match") or {}

life = []
if (raw / "lifecycle.tsv").exists():
    for line in (raw / "lifecycle.tsv").read_text().splitlines():
        parts = line.split("\t")
        if len(parts) == 3:
            life.append(parts)
life_fail = [s for s, r, _ in life if r == "fail"]


def pct(a, b):
    if a in (None, 0) or b is None:
        return None
    return (b - a) / a * 100.0


def fmt(v, digits=1):
    if v is None:
        return "-"
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    return f"{v:.{digits}f}"


def levels(d):
    return {lv["concurrency"]: lv["summary"] for lv in (d.get("concurrency") or {}).get("levels") or []}


def points(d):
    return {p["label"]: p["summary"] for p in (d.get("context_sweep") or {}).get("points") or []}


ctx_differs = {d["key"][1] for d in match.get("differs", []) if d["key"][0] == "ctx"}
rows, flagged = [], []


def row(point, metric, a, b, better, digits=1, note_if_differs=False, judge=True):
    ch = pct(a, b)
    worse = judge and ch is not None and ((better == "higher" and ch < -thr) or (better == "lower" and ch > thr))
    note = f"{ch:+.1f}%" if ch is not None else "-"
    if note_if_differs and point in ctx_differs:
        note += " (different replies)"
    elif worse:
        note += " **slower**" if better == "higher" or "TTFT" in metric else " **worse**"
        flagged.append(f"{point} {metric} {note}")
    rows.append(f"| {point} | {metric} | {fmt(a, digits)} | {fmt(b, digits)} | {note} |")


lo, ln = levels(R["old"]), levels(R["new"])
for c in sorted(set(lo) & set(ln)):
    row(f"C{c}", "aggregate tok/s", lo[c]["aggregate_tps"]["median"], ln[c]["aggregate_tps"]["median"], "higher")
for c in sorted(set(lo) & set(ln)):
    row(f"C{c}", "TTFT p50 s", lo[c]["ttft_p50_s"], ln[c]["ttft_p50_s"], "lower", 3)
    row(f"C{c}", "TTFT p95 s", lo[c]["ttft_p95_s"], ln[c]["ttft_p95_s"], "lower", 3, judge=False)
    if lo[c].get("draft_acceptance") is not None or ln[c].get("draft_acceptance") is not None:
        a, b = lo[c].get("draft_acceptance"), ln[c].get("draft_acceptance")
        rows.append(f"| C{c} | draft acceptance | {fmt(a and a * 100)}% | {fmt(b and b * 100)}% | |")
po, pn = points(R["old"]), points(R["new"])
order = sorted(set(po) & set(pn), key=lambda s: float(s.rstrip("k")) if s.rstrip("k").replace(".", "").isdigit() else 0)
for p in order:
    row(p, "decode tok/s", po[p]["decode_tps"]["median"], pn[p]["decode_tps"]["median"], "higher", 1, True)
for p in order:
    row(p, "prefill tok/s", po[p]["prompt_tps"]["median"], pn[p]["prompt_tps"]["median"], "higher")
for p in order:
    row(p, "TTFT s", po[p]["ttft_s"]["median"], pn[p]["ttft_s"]["median"], "lower", 3)
for p in order:
    row(p, "peak memory GiB", po[p]["memory_peak_gib"]["max"], pn[p]["memory_peak_gib"]["max"], "lower")

# Anomalies
anomalies = []
for side, v in (("old", old), ("new", new)):
    u = (match_report.get("usage") or {}).get(side)
    if not u:
        anomalies.append(f"{label} {v}: no benchmark results")
        continue
    if u["errors"]:
        anomalies.append(f"{label} {v}: {u['errors']} request errors, e.g. {u['error_samples'][:2]}")
    if u["usage_missing"]:
        anomalies.append(f"{label} {v}: {u['usage_missing']} requests without usage")
    if u["short_concurrency_replies"]:
        anomalies.append(f"{label} {v}: {u['short_concurrency_replies']} concurrency replies shorter than max_tokens")
    per = (match_report.get("per_pass") or {}).get(side) or {}
    for c, vals in per.items():
        if len(vals) > 1 and min(vals) > 0 and (max(vals) - min(vals)) / min(vals) * 100 > thr:
            anomalies.append(f"{label} {v} C{c}: passes differ by more than {thr:g}% "
                             f"({' / '.join(f'{x:.1f}' for x in vals)} tok/s), noisy point")
health = {"samples": 0, "bad": 0}
for p in raw.glob("health-*.log"):
    for line in p.read_text().splitlines():
        parts = line.split()
        if len(parts) == 2:
            health["samples"] += 1
            health["bad"] += parts[1] != "200"
if health["bad"]:
    anomalies.append(f"engine /health: {health['bad']} of {health['samples']} samples under load were not 200")
role_log = (raw / "role.log").read_text(errors="replace") if (raw / "role.log").exists() else ""
errs = len(re.findall(r'"level":\s*"(?:error|ERROR)"', role_log))
warns = len(re.findall(r'"level":\s*"(?:warn|WARN)', role_log))
if errs or warns:
    anomalies.append(f"role log: {errs} error and {warns} warning lines (raw/role.log)")
drained = (raw / "drained.txt").read_text().strip() if (raw / "drained.txt").exists() else ""
if drained and '"drained":true' not in drained:
    anomalies.append(f"role shutdown not drained: {drained}")

startups = []
if (raw / "startup.txt").exists():
    for line in (raw / "startup.txt").read_text().splitlines():
        n, l, s = line.split()
        startups.append((n, l, float(s)))
cold = {l: s for _, l, s in startups if l.startswith("cold-")}
warm = [s for _, l, s in startups if l.startswith("warm")]

if life_fail:
    verdict = f"**FAIL.** {len(life_fail)} lifecycle step(s) failed: {', '.join(life_fail)}."
elif not R["old"] or not R["new"]:
    verdict = "**INCOMPLETE.** The benchmark did not produce results for both versions."
elif flagged or any("request errors" in a for a in anomalies):
    verdict = (f"**CHECK.** The lifecycle passes, but {len(flagged)} point(s) are more than {thr:g}% "
               f"worse on {new} or requests failed; see Speed and Anomalies.")
else:
    verdict = (f"**PASS.** {label} {new} looks like a drop-in replacement for {old} on this setup: "
               f"the CapyCTL lifecycle passes and no measured point is more than {thr:g}% worse.")

title = f"{cfg['model_title']}: {label} {old} vs {new}"
lines = [f"# Verdict: {title}", "",
         f"One {(R['new'].get('meta') or {}).get('gpu', 'GPU')} through CapyCTL standalone "
         f"(`{cfg['capyctl_commit'][:9]}`). Pinned versions and commands are in `versions.md`; "
         "charts and tables in `report/`.", "", verdict, ""]
lines += ["## Lifecycle", "", "| Step | Result | Detail |", "|---|---|---|"]
lines += [f"| {s} | {r} | {d.replace('|', '/')} |" for s, r, d in life]
lines += [""]
if cold or warm:
    parts = [f"cold {k[5:]} {v:.1f} s" for k, v in cold.items()]
    if warm:
        parts.append(f"warm {min(warm):.1f} to {max(warm):.1f} s")
    lines += [f"Start times (`start deployment --wait`): {', '.join(parts)}. "
              f"\"cold old\" is {old}, \"cold new\" is {new}; cold is the first start in a fresh state "
              "directory (TensorFold also builds kernels on its first start of a version).", ""]
lines += ["## Speed", "",
          f"Medians. Concurrency: {cfg['concurrency']} streams, 512 tokens, {cfg['rounds']} rounds per pass, "
          f"{cfg['passes']} interleaved passes per version ({old}, {new}, ...) pooled. "
          + (f"Context: {cfg['sweep']}, 128 tokens, {cfg['runs']} run(s) each, one pass per version. "
             if cfg["sweep"] else "No context sweep. ")
          + f"Points more than {thr:g}% worse on {new} are marked (medians and p50 only; p95 over a few "
            "rounds is too noisy to judge).", "",
          f"| Point | Metric | {old} | {new} | Change |", "|---|---|---|---|---|"] + rows + [""]
lines += ["## Same output", ""]
if match:
    kind = {"token_sha": "TensorFold's `token_sha` (hash of the generated token ids)",
            "length": "reply length only (completion tokens and bytes; the engine reports no token hash)",
            "mixed": "a mix of `token_sha` and reply length"}[match["kind"]]
    lines += [f"Compared by {kind}. {match['identical']}/{match['keys']} keys identical between the versions "
              f"and stable within each: {match['concurrency'][0]}/{match['concurrency'][1]} concurrency "
              f"(C, prompt) pairs, {match['context'][0]}/{match['context'][1]} context runs. "
              f"Stable within {old}: {match['stable_within']['old'][0]}/{match['stable_within']['old'][1]}; "
              f"within {new}: {match['stable_within']['new'][0]}/{match['stable_within']['new'][1]}.", ""]
    for d in match["differs"][:12]:
        lines.append(f"- differs: {' '.join(d['key'])}")
    if match["differs"]:
        lines += ["", "Different replies between releases can be expected when the engine changed its "
                  "kernels' arithmetic; check the release notes before calling it a regression.", ""]
else:
    lines += ["No comparison (no results).", ""]
lines += ["## Anomalies", ""]
lines += [f"- {a}" for a in anomalies] or ["None found."]
lines += [""]
(out / "verdict.md").write_text("\n".join(lines))

# versions.md
vt = {}
if (raw / "versions.txt").exists():
    for line in (raw / "versions.txt").read_text().splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            vt[k] = v
vrows = [("CapyCTL", f"{vt.get('capyctl_version', '?')}, commit `{cfg['capyctl_commit']}`, release build on the host "
          f"from a snapshot of the checkout (`scripts/live/matrix/sync.sh`), snapshot digest `{cfg['snapshot']}`")]
for side, v in (("old", old), ("new", new)):
    vrows.append((f"{label} {v}", f"reports `{vt.get(side + '_reported', '?')}`, `{vt.get(side + '_source', '?')}` "
                  f"(`{vt.get(side + '_venv', '?')}`), Python {vt.get(side + '_python', '?')}, "
                  f"torch {vt.get(side + '_torch', '?')}, triton {vt.get(side + '_triton', '?')}"))
vrows += [("Model", f"`{cfg['model_ref']}`"),
          ("Drafter", f"`{cfg['drafter_ref']}`" if cfg["drafter_ref"] else "none"),
          ("Benchmark tool", f"capyctl-bench, capyctl-recipes `tools/capyctl-bench` at `{cfg['bench_commit']}`"),
          ("GPU", f"one NVIDIA {vt.get('gpu', '?')}, driver {vt.get('driver', '?')}"),
          ("CUDA toolkit", vt.get("cuda") or "none in /usr/local/cuda"),
          ("Kernel", vt.get("kernel", "?"))]
deps = sorted((raw / "deployments").glob("*.yaml")) if (raw / "deployments").exists() else []
vlines = ["# Pinned versions", "", "| Component | Version |", "|---|---|"]
vlines += [f"| {k} | {v} |" for k, v in vrows]
vlines += ["", "## Command", "", "```bash", cfg["command"], "```", "",
           "The deployment files the run used are in `raw/deployments/` "
           f"({', '.join(p.name for p in deps) or 'none'}), with the host's home directory "
           "written as `~`.", ""]
(out / "versions.md").write_text("\n".join(vlines))
print((out / "verdict.md").read_text())
