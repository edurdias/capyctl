#!/usr/bin/env python3
"""Fold one M16 evidence directory into summary.json (plan unit W2).

usage: m16_report.py --evid DIR --fixture NAME [--goldens FILE]

Reads marks.txt (control-host epoch ms), mem-samples.txt (host epoch ms, MemAvailable
kB, compute apps "pid,MiB;"), requests.jsonl, i1.jsonl, load summaries and the
accounting snapshots. Reports Ready time, the checkpoint digest row, answers,
the MemAvailable baseline and drops (at Ready, peak over the run, peak under
load), the GPU compute memory at Ready and under load, and the P2 suggestion
(gen_budgets.py suggest: peak drop plus 10 %, rounded up to a GiB). It judges
nothing: a row is judged from the evidence against the matrix.
"""

import argparse
import json
import math
import os
import statistics

GIB = 1 << 30


def read_json(path):
    try:
        with open(path) as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def read_jsonl(path):
    try:
        with open(path) as handle:
            return [json.loads(line) for line in handle if line.strip()]
    except OSError:
        return []


def marks(path):
    result = {}
    try:
        with open(path) as handle:
            for line in handle:
                name, value = line.split()
                result.setdefault(name, int(value))
    except OSError:
        pass
    return result


def samples(path):
    out = []
    try:
        with open(path) as handle:
            for line in handle:
                parts = line.split()
                if len(parts) < 2 or not parts[0].isdigit() or not parts[1].isdigit():
                    continue
                apps = parts[2] if len(parts) > 2 else ""
                gpu = 0
                for app in filter(None, apps.split(";")):
                    fields = app.split(",")
                    if len(fields) == 2 and fields[1].isdigit():
                        gpu += int(fields[1])
                out.append((int(parts[0]), int(parts[1]), gpu))
    except OSError:
        pass
    return out


def window(rows, start, end):
    return [r for r in rows if (start is None or r[0] >= start) and (end is None or r[0] <= end)]


def nearest(rows, t):
    return min(rows, key=lambda r: abs(r[0] - t)) if rows and t else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--evid", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--goldens")
    args = parser.parse_args()
    e = args.evid
    m = marks(os.path.join(e, "marks.txt"))
    rows = samples(os.path.join(e, "mem-samples.txt"))
    fixture = read_json(os.path.join(e, "fixture.json")) or {}
    config = fixture.get("engine_config", {})
    declared = config.get("memory", {}).get("request", "")
    declared_bytes = int(declared[:-1]) if declared.endswith("B") and declared[:-1].isdigit() else None
    summary = {"fixture": args.fixture, "engine": fixture.get("runtime_profile"), "engine_config": config,
               "model_path": (fixture.get("model") or {}).get("source", {}).get("path")}

    deploy, ready = m.get("deploy"), m.get("ready")
    summary["ready"] = ready is not None
    summary["ready_seconds"] = round((ready - deploy) / 1000, 1) if ready and deploy else None

    acc = read_json(os.path.join(e, "accounting-ready.json")) or read_json(os.path.join(e, "accounting-failed.json")) or {}
    digests = acc.get("checkpoint_digests") or []
    if digests:
        d = digests[-1]
        summary["checkpoint_digest"] = {
            "state": d.get("state"), "digest": d.get("digest"), "weights_bytes": d.get("weights_bytes"),
            "provisional": d.get("provisional"), "diagnostic": d.get("diagnostic"),
            "recorded_after_deploy_seconds": round((d["updated_at_ms"] - deploy) / 1000, 1)
            if d.get("state") != "pending" and deploy else None}

    requests = read_jsonl(os.path.join(e, "requests.jsonl"))
    answers = []
    for r in requests:
        content = r.get("content") or ""
        answers.append({"prompt": r.get("prompt"), "status": r.get("status"), "stream": r.get("stream"),
                        "verdict": r.get("verdict"), "expect": r.get("expect"),
                        "sse_well_formed": r.get("sse_well_formed"), "elapsed_s": r.get("elapsed_s"),
                        "finish_reason": r.get("finish_reason"), "content_chars": len(content),
                        "thinks": "<think>" in content or "</think>" in content,
                        "answer_tail": content.split("</think>")[-1].strip()[-120:]})
    summary["answers"] = answers

    i1 = read_jsonl(os.path.join(e, "i1.jsonl"))
    if i1:
        last = i1[-1]
        summary["i1"] = {"mode": last.get("i1"), "status": last.get("status"), "tokens": len(last.get("tokens") or []),
                         "logprobs_forwarded": bool(last.get("tokens")), "content": last.get("content"),
                         "first_mismatch": last.get("first_mismatch"), "max_logprob_delta": last.get("max_logprob_delta")}
        if not last.get("tokens") and last.get("status") == 200:
            summary["i1"]["finding"] = "routed answer carried no per-token logprobs; greedy text is the golden"
    loads = [read_json(os.path.join(e, f)) for f in sorted(os.listdir(e)) if f.startswith("load-") and f.endswith(".summary.json")]
    summary["load"] = [x for x in loads if x]

    before = window(rows, None, deploy)
    if before:
        baseline = int(statistics.median(r[1] for r in before))
        run = window(rows, deploy, m.get("stop"))
        at_ready = nearest(window(rows, deploy, None), ready)
        loaded = window(rows, m.get("load_start"), m.get("load_end"))
        mem = {"baseline_mem_available_kb": baseline, "samples": len(rows)}
        if run:
            low = min(run, key=lambda r: r[1])
            mem["peak_drop_gib"] = round((baseline - low[1]) * 1024 / GIB, 2)
            mem["peak_at_seconds_after_deploy"] = round((low[0] - deploy) / 1000, 1)
            mem["max_gpu_compute_mib"] = max(r[2] for r in run)
        if at_ready:
            mem["ready_drop_gib"] = round((baseline - at_ready[1]) * 1024 / GIB, 2)
            mem["ready_gpu_compute_mib"] = at_ready[2]
        if loaded:
            mem["load_drop_gib"] = round((baseline - min(r[1] for r in loaded)) * 1024 / GIB, 2)
            mem["load_gpu_compute_mib_max"] = max(r[2] for r in loaded)
        after = window(rows, m.get("stopped"), None)
        if after:
            mem["after_stop_drop_gib"] = round((baseline - after[-1][1]) * 1024 / GIB, 2)
            mem["after_stop_gpu_compute_mib"] = after[-1][2]
        if run and baseline > low[1]:
            peak = (baseline - low[1]) * 1024
            mem["suggested_request_bytes"] = math.ceil(peak * 1.10 / GIB) * GIB
            mem["suggested_request_gib"] = mem["suggested_request_bytes"] // GIB
        if declared_bytes:
            mem["declared_request_gib"] = round(declared_bytes / GIB, 2)
        summary["memory"] = mem

    stopped = read_json(os.path.join(e, "accounting-stopped.json")) or {}
    loaded_acc = read_json(os.path.join(e, "accounting-loaded.json")) or {}
    summary["request_leases_after_traffic"] = {
        "deployment": loaded_acc.get("request_leases_deployment"), "total": loaded_acc.get("request_leases_total")}
    summary["after_stop"] = {k: stopped.get(k) for k in ("reservations", "lifecycle_claims", "request_leases_deployment",
                                                        "request_leases_total", "retained_bindings", "endpoint_leases",
                                                        "deployment")}
    try:
        with open(os.path.join(e, "cleanup-host.txt")) as handle:
            summary["cleanup_host_leftovers"] = [line.strip() for line in handle if "LEFTOVER" in line]
    except OSError:
        summary["cleanup_host_leftovers"] = None
    try:
        with open(os.path.join(e, "cleanup-identities.txt")) as handle:
            outcomes = [json.loads(line)["outcome"] for line in handle if line.strip().startswith("{")]
        summary["owned_identities_after_stop"] = outcomes
    except OSError:
        summary["owned_identities_after_stop"] = None
    print(json.dumps(summary, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
