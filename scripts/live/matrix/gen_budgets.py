#!/usr/bin/env python3
"""Per-host budget policies for the matrix (decision D10, matrix section 3).

  gen_budgets.py write  [--measured FILE] [--out-dir DIR]   write normal.yaml and tight.yaml
  gen_budgets.py check  [--measured FILE] [--mem-available-kb N]  print the fit table
  gen_budgets.py suggest --model K --baseline-kb N --ready-kb M [--headroom 0.10]
                                   P2: a memory request from a measured MemAvailable drop

Normal: managed limit floor(0.8 C), free reserve ceil(0.1 C), max_parked 2.
Tight: admits q30 alone, or one of q27f/q14 with q4, never two of {q30, q27f, q14};
78 GiB with the proposed allocations, recomputed by the same rule from measured
requests. Output files are JSON, which is valid strict YAML. No live effect.

Co-residence (2026-09-23): each model's `co` variant (context 8192, smaller KV
cache) must let q30+q4 and q27f+q14+q4 fit the normal budget together; `check`
fails when they do not. It also prints, as advice only, each co variant's
placeholder startup peak (max(request, weights x 1.6 + 8 GiB), the store's
default until a first run measures it) and an order in which the planned
co-resident set can start one at a time on the per-host activation gate.
"""

import argparse
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CAPACITY = 130_663_170_048  # reported GB10 capacity, both hosts (matrix section 3)
GIB = 1 << 30
PARKED_LIMIT = 32_665_792_500  # Phase B host documents
HOST_KV_LIMIT = 13_066_317_000
TIGHT_PREFERRED = 78 * GIB
STARTUP_FACTOR = (8, 5)  # owner decision 2026-09-23: placeholder weights x 1.6
MARGIN = 8 * GIB  # ADR 0014 section 5 placeholder per-engine margin
ANCHOR = "27f"  # D5 catalog anchor, qwen3.8-27b-nvfp4


def load_models(measured_path=None):
    with open(os.path.join(HERE, "models.json")) as handle:
        catalog = json.load(handle)
    models = {}
    for key, spec in catalog["models"].items():
        if "extends" in spec:
            merged = dict(catalog["models"][spec["extends"]])
            merged.update({k: v for k, v in spec.items() if k != "extends"})
            spec = merged
        models[key] = dict(spec)
    if measured_path:
        with open(measured_path) as handle:
            measured = json.load(handle)
        for key, values in measured.items():
            if key.startswith("_"):
                continue
            if key not in models:
                sys.exit(f"measured.json names unknown model {key}")
            for field in ("request_bytes", "kv_cache_bytes"):
                if field in values:
                    models[key][field] = int(values[field])
            models[key]["measured"] = True
    return models


def co_models(models):
    """Each model's co-residence variant: its base spec with the `co` overrides."""
    variants = {}
    for key, spec in models.items():
        if "co" not in spec:
            continue
        variant = {k: v for k, v in spec.items() if k != "co"}
        variant.update(spec["co"])
        variants[key] = variant
    return variants


def startup_bytes(spec):
    """The startup peak the store reserves before a first run measures one:
    a declared co.startup_bytes, else max(request, weights x 1.6 + margin)."""
    if spec.get("startup_bytes"):
        return int(spec["startup_bytes"])
    numerator, denominator = STARTUP_FACTOR
    weights = spec.get("weights_bytes_estimate", 0)
    return max(spec["request_bytes"], weights * numerator // denominator + MARGIN)


def startup_order(limit, members, models):
    """An order in which `members` can start one at a time, each startup peak
    beside the steady requests of those already Ready; None when none exists."""
    from itertools import permutations

    for order in permutations(members):
        ready = 0
        for member in order:
            if startup_bytes(models[member]) + ready > limit:
                break
            ready += models[member]["request_bytes"]
        else:
            return list(order)
    return None


def normal_policy():
    return {
        "policy": "normal",
        "capacity_bytes": CAPACITY,
        "managed_limit": f"{CAPACITY * 8 // 10}B",
        "free_reserve": f"{math.ceil(CAPACITY / 10)}B",
        "parked_limit": f"{PARKED_LIMIT}B",
        "host_kv_limit": f"{HOST_KV_LIMIT}B",
        "max_parked": 2,
    }


def tight_bounds(models):
    # The anchor is the NVFP4 build (27f); 27 is the bf16 checkpoint of the same model.
    q4, q14, q27, q30 = (models[k]["request_bytes"] for k in ("4", "14", ANCHOR, "30"))
    lower = max(q30, q27 + q4, q14 + q4)  # must admit each of these
    upper = min(q30 + q27, q30 + q14, q27 + q14)  # must refuse every one of these
    return lower, upper


def tight_policy(models):
    lower, upper = tight_bounds(models)
    if lower >= upper:
        sys.exit(f"no tight limit exists: must admit {lower} B but refuse {upper} B")
    if lower <= TIGHT_PREFERRED < upper:
        limit = TIGHT_PREFERRED
    else:
        limit = ((lower + upper) // 2) // GIB * GIB
        if not lower <= limit < upper:
            limit = lower
    policy = normal_policy()
    policy.update({"policy": "tight", "managed_limit": f"{limit}B", "max_parked": 1})
    return policy


def fits(limit, members, models):
    return sum(models[m]["request_bytes"] for m in members) <= limit


def check(models, mem_available_kb=None):
    normal = int(normal_policy()["managed_limit"][:-1])
    tight = int(tight_policy(models)["managed_limit"][:-1])
    reserve = int(normal_policy()["free_reserve"][:-1])
    rows = []
    for key in ("4", "14", "27", "27f", "30"):
        spec = models[key]
        weights = spec.get("weights_bytes_estimate", 0)
        resolution_ok = weights + spec["kv_cache_bytes"] <= spec["request_bytes"]
        advisory_ok = weights + spec["kv_cache_bytes"] + MARGIN <= spec["request_bytes"]
        rows.append(f"q{key:<4} request {spec['request_bytes'] / GIB:6.2f} GiB  kv {spec['kv_cache_bytes'] / GIB:5.2f} GiB  "
                    f"weights~{weights / GIB:6.2f} GiB  resolution {'ok' if resolution_ok else 'REFUSED'}  "
                    f"with-margin {'ok' if advisory_ok else 'over'}{'  (measured)' if spec.get('measured') else ''}  "
                    f"startup~{startup_bytes(spec) / GIB:6.2f} GiB{'' if startup_bytes(spec) <= normal else '  (exceeds normal limit)'}")
    co = co_models(models)
    for key in ("4", "14", "27", "27f", "30"):
        if key not in co:
            continue
        spec = co[key]
        weights = spec.get("weights_bytes_estimate", 0)
        static_ok = weights + spec["kv_cache_bytes"] + MARGIN <= spec["request_bytes"]
        rows.append(f"q{key + 'co':<6} request {spec['request_bytes'] / GIB:6.2f} GiB  kv {spec['kv_cache_bytes'] / GIB:5.2f} GiB  "
                    f"context {spec['context_length']}  with-margin {'ok' if static_ok else 'over'}  "
                    f"startup~{startup_bytes(spec) / GIB:6.2f} GiB{'' if startup_bytes(spec) <= normal else '  (exceeds normal limit: declare co.startup_bytes or measure)'}")
    expectations = [
        ("normal", normal, ["30", "14"], False),
        ("tight", tight, ["30"], True), ("tight", tight, [ANCHOR, "4"], True), ("tight", tight, ["14", "4"], True),
        ("tight", tight, ["30", "14"], False), ("tight", tight, ["30", ANCHOR], False), ("tight", tight, [ANCHOR, "14"], False),
    ]
    failed = False
    for name, limit, members, expected in expectations:
        got = fits(limit, members, models)
        failed |= got != expected
        rows.append(f"{name:6} {'+'.join('q' + m for m in members):14} {'fits' if got else 'refused':8} "
                    f"expected {'fits' if expected else 'refused'}{'' if got == expected else '  MISMATCH'}")
    # Co-residence: the planned sets, steady, with the co variants.
    for members in (["30", "4"], [ANCHOR, "14", "4"]):
        if not all(m in co for m in members):
            failed = True
            rows.append(f"co     {'+'.join('q' + m for m in members)}: no co variant  MISMATCH")
            continue
        got = fits(normal, members, co)
        failed |= not got
        total = sum(co[m]["request_bytes"] for m in members)
        order = startup_order(normal, members, co)
        rows.append(f"normal {'+'.join('q' + m + 'co' for m in members):18} {'fits' if got else 'refused':8} "
                    f"expected fits  {total / GIB:.2f} of {normal / GIB:.2f} GiB{'' if got else '  MISMATCH'}  "
                    f"startup order (placeholder peaks): {'+'.join('q' + m for m in order) if order else 'none'}")
    if mem_available_kb:
        available = mem_available_kb * 1024
        for key in ("4", "14", "27", "27f", "30"):
            need = models[key]["request_bytes"] + reserve
            rows.append(f"agent check q{key}: request + free reserve {need / GIB:.2f} GiB vs MemAvailable "
                        f"{available / GIB:.2f} GiB -> {'ok' if need <= available else 'REJECTED'}")
    print("\n".join(rows))
    print(f"normal managed_limit {normal} B; tight managed_limit {tight} B; tight bounds {tight_bounds(models)}")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("write")
    w.add_argument("--measured")
    w.add_argument("--out-dir", default=os.path.join(HERE, "budgets"))
    c = sub.add_parser("check")
    c.add_argument("--measured")
    c.add_argument("--mem-available-kb", type=int)
    s = sub.add_parser("suggest")
    s.add_argument("--model", required=True)
    s.add_argument("--baseline-kb", type=int, required=True, help="MemAvailable before launch")
    s.add_argument("--ready-kb", type=int, required=True, help="lowest MemAvailable at Ready or under load")
    s.add_argument("--headroom", type=float, default=0.10)
    args = parser.parse_args()
    if args.cmd == "write":
        models = load_models(args.measured)
        os.makedirs(args.out_dir, exist_ok=True)
        for policy in (normal_policy(), tight_policy(models)):
            path = os.path.join(args.out_dir, policy["policy"] + ".yaml")
            with open(path, "w") as handle:
                json.dump(policy, handle, indent=1)
                handle.write("\n")
            print(path, policy["managed_limit"], "max_parked", policy["max_parked"])
        return check(models)
    if args.cmd == "check":
        return check(load_models(args.measured), args.mem_available_kb)
    peak = (args.baseline_kb - args.ready_kb) * 1024
    if peak <= 0:
        sys.exit("no MemAvailable drop between the samples")
    request = math.ceil(peak * (1 + args.headroom) / GIB) * GIB
    print(json.dumps({args.model: {"measured_peak_bytes": peak, "request_bytes": request}}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
