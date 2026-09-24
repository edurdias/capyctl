#!/usr/bin/env python3
"""One routed request with its evidence record (plan unit W2, E0).

usage: infer.py --route R --prompt TEXT [--stream] [--max-tokens 32]
                [--expect SUBSTRING] --out FILE.jsonl

Records status, served `model`, answer, response headers (credential headers
dropped), timing and a client marker sent as x-request-id. HTTP 200 alone is not
evidence: with --expect the exit status is non-zero unless the answer contains
the expected marker. The API key is read from MLLM_API_KEY only.
"""

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrixhttp  # noqa: E402


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--route", required=True)
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--stream", action="store_true")
    parser.add_argument("--max-tokens", type=int, default=32)
    parser.add_argument("--expect")
    parser.add_argument("--timeout", type=float, default=900)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    record = matrixhttp.chat(args.route, [{"role": "user", "content": args.prompt}], stream=args.stream,
                             max_tokens=args.max_tokens, timeout=args.timeout)
    record["prompt"] = args.prompt
    record["expect"] = args.expect
    ok = record.get("status") == 200 and (not args.stream or record.get("sse_well_formed"))
    if args.expect is not None:
        ok = ok and args.expect in (record.get("content") or "")
    record["verdict"] = "ok" if ok else "failed"
    matrixhttp.append_jsonl(args.out, record)
    print(json.dumps({k: record.get(k) for k in ("marker", "route", "status", "model", "content", "elapsed_s", "verdict")}))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
