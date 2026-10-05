#!/usr/bin/env python3
"""Pool the interleaved passes of each version into one capyctl-bench results file,
and check that greedy outputs match within and between versions.

usage: merge_passes.py CAPYCTL_BENCH_DIR RAW_DIR OUT_DIR ENGINE OLD NEW

RAW_DIR holds conc-{old,new}-pass{1..N}.json and, when a context sweep ran,
ctx-{old,new}.json (capyctl-bench run output). OUT_DIR gets <engine>-<version>.json
for each version: the concurrency section with every pass's rounds per stream
count, re-summarized with capyctl-bench's own functions, plus the context sweep as
run; and match.json with the output comparison.

Outputs are compared by TensorFold's token_sha (a hash of the generated token
ids) when the engine reports it, else by (completion_tokens, output_bytes), a
weaker check that only catches replies of a different length.
"""
import json, statistics, sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, sys.argv[1])
import capyctl_bench as cb  # noqa: E402

raw, out = Path(sys.argv[2]), Path(sys.argv[3])
engine, versions = sys.argv[4], {"old": sys.argv[5], "new": sys.argv[6]}
out.mkdir(parents=True, exist_ok=True)


def fingerprint(r):
    sha = ((r.get("extras") or {}).get("tensorfold") or {}).get("token_sha")
    if sha:
        return "token_sha", sha
    return "length", f"{r.get('completion_tokens')}/{r.get('output_bytes')}"


shas, kinds, report = {}, set(), {"versions": versions, "per_pass": {}, "usage": {}}
for side in ("old", "new"):
    paths = sorted(raw.glob(f"conc-{side}-pass*.json"))
    passes = [json.loads(p.read_text()) for p in paths]
    ctx_path = raw / f"ctx-{side}.json"
    ctx = json.loads(ctx_path.read_text()) if ctx_path.exists() else None
    if not passes and not ctx:
        print(f"{side}: no results", file=sys.stderr)
        continue
    merged = json.loads(json.dumps(passes[0] if passes else ctx))
    if passes:
        for i, lv in enumerate(merged["concurrency"]["levels"]):
            rounds = []
            for p, data in enumerate(passes, start=1):
                other = data["concurrency"]["levels"][i]
                assert other["concurrency"] == lv["concurrency"]
                for rnd in other["rounds"]:
                    rounds.append(dict(rnd, round=len(rounds), **{"pass": p}))
            lv["rounds"] = rounds
            cb.summarize_level(lv)
        merged["concurrency"]["settings"]["rounds"] = sum(d["concurrency"]["settings"]["rounds"] for d in passes)
        merged["started_at"], merged["finished_at"] = passes[0]["started_at"], passes[-1]["finished_at"]
        merged["meta"]["passes"] = f"{len(passes)}, interleaved old, new; each a fresh warm start"
        if all(d.get("memory") for d in passes):
            merged["memory"] = {"unit": "GiB", "interval_s": passes[0]["memory"]["interval_s"],
                                "errors": sum(d["memory"]["errors"] for d in passes),
                                "samples": [s for d in passes for s in d["memory"]["samples"]]}
    else:
        merged["concurrency"] = None
    merged["context_sweep"] = ctx["context_sweep"] if ctx else None
    (out / f"{engine}-{versions[side]}.json").write_text(json.dumps(merged, indent=1) + "\n")

    print(f"{merged['label']}: aggregate tok/s per C (each pass / pooled)")
    per_pass = {}
    for i, lv in enumerate((merged.get("concurrency") or {}).get("levels") or []):
        per = [statistics.median(r["aggregate_tps"] or 0 for r in d["concurrency"]["levels"][i]["rounds"])
               for d in passes]
        per_pass[lv["concurrency"]] = per
        print(f"  C{lv['concurrency']}: {' / '.join(f'{v:.1f}' for v in per)} / "
              f"{lv['summary']['aggregate_tps']['median'] or 0:.1f}")
    report["per_pass"][side] = per_pass
    reqs = [r for lv in (merged.get("concurrency") or {}).get("levels") or []
            for rnd in lv["rounds"] for r in rnd["requests"]]
    reqs += [r for p in (merged.get("context_sweep") or {}).get("points") or [] for r in p["requests"]]
    max_tokens = (merged.get("concurrency") or {}).get("settings", {}).get("max_tokens")
    usage = {"requests": len(reqs),
             "usage_missing": sum(1 for r in reqs if r.get("usage_missing")),
             "errors": sum(1 for r in reqs if r.get("error")),
             "short_concurrency_replies": sum(
                 1 for lv in (merged.get("concurrency") or {}).get("levels") or []
                 for rnd in lv["rounds"] for r in rnd["requests"]
                 if max_tokens and r.get("completion_tokens") != max_tokens),
             "finish_reasons": sorted({str(r.get("finish_reason")) for r in reqs}),
             "error_samples": sorted({str(r.get("error"))[:160] for r in reqs if r.get("error")})[:5]}
    report["usage"][side] = usage
    print(f"  {usage['requests']} measured requests, {usage['usage_missing']} usage_missing, "
          f"{usage['errors']} errors, finish_reason {usage['finish_reasons']}")
    s = defaultdict(set)
    for lv in (merged.get("concurrency") or {}).get("levels") or []:
        for rnd in lv["rounds"]:
            for r in rnd["requests"]:
                kind, fp = fingerprint(r)
                kinds.add(kind)
                s[(f"C{lv['concurrency']}", r["prompt_id"])].add(fp)
    for p in (merged.get("context_sweep") or {}).get("points") or []:
        for r in p["requests"]:
            kind, fp = fingerprint(r)
            kinds.add(kind)
            s[("ctx", p["label"], r["run"])].add(fp)
    shas[side] = s

a, b = shas.get("old", {}), shas.get("new", {})
keys = sorted(set(a) | set(b), key=str)
stable = {side: sum(1 for k in d if len(d[k]) == 1) for side, d in (("old", a), ("new", b))}
same = [k for k in keys if a.get(k) == b.get(k) and len(a.get(k, ())) == 1]
conc_keys = [k for k in keys if k[0] != "ctx"]
match = {
    "kind": "token_sha" if kinds == {"token_sha"} else "length" if kinds == {"length"} else "mixed",
    "keys": len(keys),
    "identical": len(same),
    "concurrency": [len([k for k in same if k[0] != "ctx"]), len(conc_keys)],
    "context": [len([k for k in same if k[0] == "ctx"]), len(keys) - len(conc_keys)],
    "stable_within": {"old": [stable["old"], len(a)], "new": [stable["new"], len(b)]},
    "differs": [{"key": list(map(str, k)), "old": sorted(map(str, a.get(k, ()))),
                 "new": sorted(map(str, b.get(k, ())))} for k in keys if k not in same],
}
report["match"] = match
(out / "match.json").write_text(json.dumps(report, indent=1) + "\n")
print(f"{match['kind']}: {len(same)}/{len(keys)} keys identical between versions and stable within each "
      f"({match['concurrency'][0]}/{match['concurrency'][1]} concurrency (C, prompt), "
      f"{match['context'][0]}/{match['context'][1]} context runs); stable within old "
      f"{stable['old']}/{len(a)}, new {stable['new']}/{len(b)}")
for d in match["differs"]:
    print("  differs:", d["key"], d["old"], d["new"])
