#!/usr/bin/env python3
"""I1 model-identity probe (matrix section 3; plan unit W2).

A fixed greedy probe (temperature 0, 32 tokens, top-1 logprobs) through the
router, compared with the expected model's golden, captured at its first Ready
in the run. Goldens of the four models must differ pairwise or the row is void.

  identity_probe.py capture  --route R --model K --goldens FILE --out FILE.jsonl [--engine E] [--replace]
  identity_probe.py check    --route R --model K --goldens FILE --out FILE.jsonl [--engine E]
                             [--prefix N] [--logprob-tol X]
  identity_probe.py distinct --goldens FILE

Goldens are keyed by model (4, 14, 27, 27f, 30); with --engine they are keyed
model@engine, because vLLM and SGLang kernels may pick a different greedy token
for the same checkpoint. check passes when the first --prefix tokens (default:
all) equal the golden's and each compared logprob is within --logprob-tol.
The checkpoint-digest half of I1 is ledger.py fingerprint against
checkpoints.json. The API key comes from CAPYCTL_API_KEY only.
"""

import argparse
import fcntl
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import matrixhttp  # noqa: E402

# 27 (bf16) and 27f (NVFP4) are different checkpoints, so both must be distinct.
BASE_MODELS = ("4", "14", "27", "27f", "30")


def probe_settings():
    with open(os.path.join(HERE, "models.json")) as handle:
        return json.load(handle)["identity_probe"]


def probe(route):
    settings = probe_settings()
    record = matrixhttp.chat(route, [{"role": "user", "content": settings["prompt"]}], stream=False,
                             max_tokens=settings["max_tokens"], temperature=0.0,
                             extra={"logprobs": True, "top_logprobs": 1})
    tokens, logprobs = [], []
    content = (record.get("logprobs") or {}).get("content") or []
    for item in content:
        tokens.append(item.get("token"))
        logprobs.append(item.get("logprob"))
    record.pop("logprobs", None)
    record.update({"probe_prompt": settings["prompt"], "tokens": tokens, "token_logprobs": logprobs})
    return record


def load(path):
    if os.path.exists(path):
        with open(path) as handle:
            return json.load(handle)
    return {}


def save(path, goldens):
    tmp = path + ".tmp"
    with open(tmp, "w") as handle:
        json.dump(goldens, handle, indent=1, sort_keys=True)
    os.replace(tmp, path)


def key_of(args):
    return f"{args.model}@{args.engine}" if args.engine else args.model


def capture(args):
    key = key_of(args)
    if key in load(args.goldens) and not args.replace:
        sys.exit(f"golden {key} exists; pass --replace to overwrite")
    record = probe(args.route)
    record["i1"] = "capture"
    matrixhttp.append_jsonl(args.out, record)
    if record.get("status") != 200 or not (record["tokens"] or record.get("content")):
        print(json.dumps({"i1": "capture_failed", "status": record.get("status"), "route": args.route}))
        return 1
    # No per-token logprobs in the routed answer: keep the greedy text as the
    # fallback identity and mark it, so the missing forwarding stays visible.
    logprobs_forwarded = bool(record["tokens"])
    # Rows on both hosts may capture at the same time: read-modify-write under a lock.
    lock = open(args.goldens + ".lock", "w")
    fcntl.flock(lock, fcntl.LOCK_EX)
    goldens = load(args.goldens)
    goldens[key] = {"route": args.route, "tokens": record["tokens"], "token_logprobs": record["token_logprobs"],
                    "content": record.get("content"), "captured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "marker": record["marker"], "logprobs_forwarded": logprobs_forwarded}
    save(args.goldens, goldens)
    fcntl.flock(lock, fcntl.LOCK_UN)
    print(json.dumps({"i1": "captured", "key": key, "route": args.route, "tokens": len(record["tokens"]),
                      "logprobs_forwarded": logprobs_forwarded, "content": record.get("content")}))
    return 0


def check(args):
    goldens = load(args.goldens)
    key = key_of(args)
    if key not in goldens:
        sys.exit(f"no golden {key}: capture it at the model's first Ready")
    golden = goldens[key]
    record = probe(args.route)
    prefix = args.prefix or len(golden["tokens"])
    want, got = golden["tokens"][:prefix], record["tokens"][:prefix]
    diffs = [abs(a - b) for a, b in zip(golden["token_logprobs"][:prefix], record["token_logprobs"][:prefix])
             if a is not None and b is not None]
    first_mismatch = next((i for i, (a, b) in enumerate(zip(want, got)) if a != b), None)
    if first_mismatch is None and len(got) < len(want):
        first_mismatch = len(got)
    ok = (record.get("status") == 200 and first_mismatch is None
          and (not diffs or max(diffs) <= args.logprob_tol))
    record.update({"i1": "pass" if ok else "fail", "golden_key": key, "prefix": prefix,
                   "first_mismatch": first_mismatch, "max_logprob_delta": max(diffs) if diffs else None})
    matrixhttp.append_jsonl(args.out, record)
    print(json.dumps({k: record[k] for k in ("i1", "golden_key", "route", "status", "first_mismatch", "max_logprob_delta")}))
    return 0 if ok else 1


def distinct(args):
    goldens = load(args.goldens)
    present = {}
    for key, golden in goldens.items():
        model = key.split("@")[0]
        if model in BASE_MODELS:
            # Text fallback when the golden was captured without logprobs.
            present.setdefault(model, []).append((key, tuple(golden["tokens"]) or (golden.get("content"),)))
    collisions = []
    keys = sorted(present)
    for i, a in enumerate(keys):
        for b in keys[i + 1:]:
            for ka, ta in present[a]:
                for kb, tb in present[b]:
                    if ta == tb:
                        collisions.append([ka, kb])
    missing = [m for m in BASE_MODELS if m not in present]
    result = {"models": keys, "missing": missing, "collisions": collisions,
              "verdict": "distinct" if not collisions else "void"}
    print(json.dumps(result))
    return 0 if not collisions else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("capture", "check"):
        p = sub.add_parser(name)
        p.add_argument("--route", required=True)
        p.add_argument("--model", required=True, choices=["4", "14", "27", "27f", "30"])
        p.add_argument("--engine", choices=["vllm", "sglang"])
        p.add_argument("--goldens", required=True)
        p.add_argument("--out", required=True)
        if name == "capture":
            p.add_argument("--replace", action="store_true")
        else:
            p.add_argument("--prefix", type=int)
            p.add_argument("--logprob-tol", type=float, default=0.25)
    d = sub.add_parser("distinct")
    d.add_argument("--goldens", required=True)
    args = parser.parse_args()
    return {"capture": capture, "check": check, "distinct": distinct}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
