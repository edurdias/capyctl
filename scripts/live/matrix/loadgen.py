#!/usr/bin/env python3
"""Concurrent load for matrix rows (plan unit W2; M02, M06, M17, M56, M57).

usage: loadgen.py --route R [--route R2 ...] --out FILE.jsonl --summary FILE.json
                  [--stream N] [--nonstream N] [--concurrency C] [--max-tokens 64]
                  [--long N --long-tokens 16384 --skew-delay 5] [--short N]
                  [--timeout 900]

Phase 1 (optional, long-prompt skew for M57): N streaming requests whose prompt
is about --long-tokens tokens start first; after --skew-delay seconds phase 2
starts. Phase 2: --stream streaming and --nonstream non-streaming requests (plus
--short short non-streaming requests), interleaved and run --concurrency at a
time, round-robin across the given routes. Every request is one evidence record;
the summary reports status counts, SSE framing, latency percentiles and the
answering host headers seen. The API key comes from CAPYCTL_API_KEY only.
"""

import argparse
import collections
import concurrent.futures
import json
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrixhttp  # noqa: E402

SHORT_PROMPTS = [
    "What is 17+25? Answer with only the number.",
    "Name the capital of France in one word.",
    "Count from 1 to 5 separated by commas.",
    "What is 9 times 7? Answer with only the number.",
]
FILLER = ("The archive lists ledgers, invoices, shipping notes and field reports from the northern "
          "depot, each one dated, signed and filed by the clerk on duty that week. ")


def long_prompt(tokens):
    # About 30 tokens per filler sentence for the Qwen tokenizers; the answer asks
    # for something only reachable after reading the whole prompt.
    repeats = max(1, tokens // 30)
    return FILLER * repeats + "\nIgnoring the text above, answer only: what is 6 times 7?"


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, int(fraction * len(ordered)))], 4)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--route", action="append", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--summary", required=True)
    parser.add_argument("--stream", type=int, default=0)
    parser.add_argument("--nonstream", type=int, default=0)
    parser.add_argument("--short", type=int, default=0)
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--max-tokens", type=int, default=64)
    parser.add_argument("--long", type=int, default=0)
    parser.add_argument("--long-tokens", type=int, default=16384)
    parser.add_argument("--skew-delay", type=float, default=5.0)
    # Output tokens of each long stream. The first live M57 (2026-09-23) used 32,
    # so the long work finished in about 13 s and never held one host's load.
    parser.add_argument("--long-max-tokens", type=int, default=32)
    # The long streams generate all --long-max-tokens tokens (`ignore_eos`), so
    # they hold their host's load; the M57 rerun (2026-09-24) saw them answer
    # the prompt's short question and end after 17 s.
    parser.add_argument("--long-ignore-eos", action="store_true")
    parser.add_argument("--timeout", type=float, default=900)
    args = parser.parse_args()
    if args.concurrency < 1 or args.concurrency > 256:
        parser.error("--concurrency must be 1..256")

    lock = threading.Lock()
    records = []

    def run(kind, index, route, prompt, stream, max_tokens):
        extra = {"ignore_eos": True} if kind == "long" and args.long_ignore_eos else None
        record = matrixhttp.chat(route, [{"role": "user", "content": prompt}], stream=stream,
                                 max_tokens=max_tokens, timeout=args.timeout, extra=extra)
        record.update({"kind": kind, "index": index})
        with lock:
            records.append(record)
            matrixhttp.append_jsonl(args.out, record)
        return record

    routes = args.route
    jobs = []
    for i in range(args.stream):
        jobs.append(("stream", i, True, SHORT_PROMPTS[i % len(SHORT_PROMPTS)], args.max_tokens))
    for i in range(args.nonstream):
        jobs.append(("nonstream", i, False, SHORT_PROMPTS[i % len(SHORT_PROMPTS)], args.max_tokens))
    for i in range(args.short):
        jobs.append(("short", i, False, SHORT_PROMPTS[i % len(SHORT_PROMPTS)], 16))
    # Interleave streaming and non-streaming so both are in flight together.
    jobs.sort(key=lambda job: (job[1], job[0]))

    started = time.time()
    long_pool = concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.long))
    long_futures = [long_pool.submit(run, "long", i, routes[i % len(routes)], long_prompt(args.long_tokens), True, args.long_max_tokens)
                    for i in range(args.long)]
    if args.long:
        time.sleep(args.skew_delay)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(run, kind, i, routes[n % len(routes)], prompt, stream, max_tokens)
                   for n, (kind, i, stream, prompt, max_tokens) in enumerate(jobs)]
        concurrent.futures.wait(futures)
    concurrent.futures.wait(long_futures)
    long_pool.shutdown()

    by_kind = collections.defaultdict(list)
    for record in records:
        by_kind[record["kind"]].append(record)
    summary = {"routes": routes, "started_unix": started, "wall_s": round(time.time() - started, 3), "kinds": {}}
    for kind, items in sorted(by_kind.items()):
        statuses = collections.Counter(str(item.get("status")) for item in items)
        hosts = collections.Counter(
            "|".join(f"{k}={v}" for k, v in sorted(item.get("headers", {}).items()) if k.startswith("x-capyctl")) or "-"
            for item in items)
        latencies = [item["elapsed_s"] for item in items if item.get("status") == 200]
        streams = [item for item in items if item.get("stream")]
        summary["kinds"][kind] = {
            "count": len(items),
            "status": dict(statuses),
            "models": dict(collections.Counter(str(item.get("model")) for item in items)),
            "routes": dict(collections.Counter(item["route"] for item in items)),
            "capyctl_headers": dict(hosts),
            "sse_well_formed": sum(1 for item in streams if item.get("sse_well_formed")),
            "sse_total": len(streams),
            "latency_p50_s": percentile(latencies, 0.5),
            "latency_p95_s": percentile(latencies, 0.95),
            "ttft_p50_s": percentile([i["ttft_s"] for i in streams if i.get("ttft_s") is not None], 0.5),
        }
    with open(args.summary, "w") as handle:
        json.dump(summary, handle, indent=1, sort_keys=True)
    print(json.dumps(summary["kinds"], sort_keys=True))
    failed = sum(1 for record in records if record.get("status") != 200
                 or (record.get("stream") and not record.get("sse_well_formed")))
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
