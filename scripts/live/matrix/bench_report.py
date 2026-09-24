#!/usr/bin/env python3
"""Aggregate M80 benchmark evidence into one model x engine x host table (SPEC section 17, T40).

usage: bench_report.py [--live target/live/matrix] [--out-md FILE] [--out-json FILE]
                       [--prompt 2048] [--concurrency 1]

Reads every target/live/matrix/M80-*/bench.json (archived `.prev-*` directories are
skipped) written by `bench.py report`. For each fixture it prints one row for the
selected cell (default: 2048-token prompt at concurrency 1) plus the peak output
throughput over all cells and the lifecycle medians (cold start, wake from park,
switch), each to first token. Numbers are measured through the shipped router and
include its path; they are workload context, never a product guarantee.
"""

import argparse
import glob
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
MODEL_ORDER = {"4": 0, "14": 1, "27": 2, "27f": 3, "30": 4}


def fmt(v, scale=1.0, digits=1):
    return "-" if v is None else f"{v * scale:.{digits}f}"


def pick_cell(cells, prompt, concurrency):
    for c in cells:
        if c.get("prompt_tokens_requested") == prompt and c.get("concurrency") == concurrency:
            return c
    return None


def row_for(report, prompt, concurrency):
    cells = report.get("cells", [])
    cell = pick_cell(cells, prompt, concurrency) or {}
    peak = max(cells, key=lambda c: (c.get("throughput") or {}).get("output_tps") or 0, default=None)
    life = report.get("lifecycle_summary", {})
    eng = cell.get("engine_side", {})
    return {
        "fixture": report.get("fixture"), "model": report.get("model"), "engine": report.get("engine"),
        "host": report.get("host"), "residency": report.get("residency"),
        "cell": cell.get("cell"), "n": cell.get("ok"),
        "ttft_p50_s": (cell.get("ttft_s") or {}).get("p50"),
        "ttft_p95_s": (cell.get("ttft_s") or {}).get("p95"),
        "prefill_tps_p50": (cell.get("prefill_tps") or {}).get("p50"),
        "decode_tps_p50": (cell.get("decode_tps") or {}).get("p50"),
        "itl_p50_s": (cell.get("itl_s") or {}).get("p50"),
        "itl_p99_s": (cell.get("itl_s") or {}).get("p99"),
        "path_overhead_ttft_mean_s": eng.get("path_overhead_ttft_mean_s"),
        "peak_output_tps": ((peak or {}).get("throughput") or {}).get("output_tps"),
        "peak_cell": (peak or {}).get("cell"),
        "cold_p50_s": (life.get("cold") or {}).get("p50"),
        "wake_p50_s": (life.get("wake") or {}).get("p50"),
        "switch_p50_s": (life.get("switch") or {}).get("p50"),
        "usage_source": cell.get("usage_source"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--live", default=os.environ.get("MLLM_MATRIX_LIVE", os.path.join(REPO, "target/live/matrix")))
    parser.add_argument("--out-md")
    parser.add_argument("--out-json")
    parser.add_argument("--prompt", type=int, default=2048)
    parser.add_argument("--concurrency", type=int, default=1)
    args = parser.parse_args()

    rows = []
    for path in sorted(glob.glob(os.path.join(args.live, "M80-*", "bench.json"))):
        if ".prev-" in path:
            continue
        with open(path) as handle:
            report = json.load(handle)
        row = row_for(report, args.prompt, args.concurrency)
        ev = os.path.dirname(os.path.abspath(path))
        row["evidence"] = os.path.relpath(ev, REPO) if ev.startswith(REPO + os.sep) else ev
        rows.append(row)
    rows.sort(key=lambda r: (MODEL_ORDER.get(str(r["model"]), 9), str(r["engine"]), str(r["host"])))

    lines = [f"# M80 aggregate ({args.prompt}-token prompt, concurrency {args.concurrency})", "",
             "Through the shipped router. Times in ms, rates in tokens/s. `ovh` is client TTFT minus the "
             "engine's own TTFT (SGLang /metrics only). Lifecycle columns are medians to first token.", "",
             "| model | engine | host | n | TTFT p50 | TTFT p95 | prefill | decode | ITL p50 | ITL p99 | ovh "
             "| peak out tok/s (cell) | cold | wake | switch | evidence |",
             "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---|"]
    for r in rows:
        lines.append(
            f"| {r['model']} | {r['engine']} | {r['host']} | {r['n'] if r['n'] is not None else '-'} "
            f"| {fmt(r['ttft_p50_s'], 1000)} | {fmt(r['ttft_p95_s'], 1000)} | {fmt(r['prefill_tps_p50'])} "
            f"| {fmt(r['decode_tps_p50'])} | {fmt(r['itl_p50_s'], 1000, 2)} | {fmt(r['itl_p99_s'], 1000, 2)} "
            f"| {fmt(r['path_overhead_ttft_mean_s'], 1000, 2)} | {fmt(r['peak_output_tps'])} ({r['peak_cell'] or '-'}) "
            f"| {fmt(r['cold_p50_s'], 1000, 0)} | {fmt(r['wake_p50_s'], 1000, 0)} | {fmt(r['switch_p50_s'], 1000, 0)} "
            f"| {r['evidence']} |")
    if not rows:
        lines.append("| (no M80 evidence found) |||||||||||||||| ")
    lines += ["", "No figure here is a product guarantee (SPEC section 17). CPU or fake runs are not evidence.", ""]
    text = "\n".join(lines)
    if args.out_md:
        with open(args.out_md, "w") as handle:
            handle.write(text)
    if args.out_json:
        with open(args.out_json, "w") as handle:
            json.dump(rows, handle, indent=1, sort_keys=True)
            handle.write("\n")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
