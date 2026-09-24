#!/usr/bin/env python3
"""Router instance selections from the server log (I3 `router_selection` lines).

usage: selections.py --log server.log [--from-line N] [--to-line M] [--deployment ID]
                     [--hosts-json hosts.json] [--max-share 0.6]

Counts, per chosen instance and host, the selections logged after line N
(`wc -l` before the load). The chosen instance is the first ranked candidate.
With --max-share the exit status is non-zero when any host got more than that
share of the selections, or when fewer than two hosts were chosen.
"""

import argparse
import collections
import json
import sys


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--log", required=True)
    p.add_argument("--from-line", type=int, default=0)
    # Lines after --to-line are not counted: a window between two `wc -l` marks
    # (M59 stall window, found live 2026-09-23: whole-run counts hid steering).
    p.add_argument("--to-line", type=int, default=0)
    p.add_argument("--deployment")
    p.add_argument("--hosts-json")
    p.add_argument("--max-share", type=float)
    p.add_argument("--min-hosts", type=int, default=0)
    a = p.parse_args()
    names = {}
    if a.hosts_json:
        listing = json.load(open(a.hosts_json))
        for h in listing.get("hosts", []):
            names[h.get("host_id") or h.get("id")] = h.get("name")
    by_host = collections.Counter()
    by_instance = collections.Counter()
    skipped = collections.Counter()
    sources = collections.Counter()
    gaps = collections.Counter()
    inputs = []
    total = 0
    with open(a.log, errors="replace") as handle:
        for n, line in enumerate(handle, 1):
            if a.to_line and n > a.to_line:
                break
            if n <= a.from_line or '"router_selection"' not in line:
                continue
            try:
                ev = json.loads(line[line.index("{"):])
            except ValueError:
                continue
            if a.deployment and ev.get("deployment") != a.deployment:
                continue
            cands = ev.get("candidates") or []
            for s in ev.get("skipped") or []:
                skipped[s.get("reason")] += 1
            if not cands:
                continue
            first = cands[0]
            if len(cands) > 1:
                gaps["chosen_lower" if (first.get("score") or 0) < (cands[1].get("score") or 0) else "tie_or_equal"] += 1
                inputs.append({"chosen": names.get(first.get("host"), first.get("host")),
                               **{names.get(c.get("host"), c.get("host")): {k: c.get(k) for k in ("score", "router_in_flight", "engine_running", "engine_waiting", "sample_age_ms")} for c in cands}})
            host = names.get(first.get("host"), first.get("host"))
            by_host[host] += 1
            by_instance[f"{first.get('instance')}@{host}"] += 1
            sources[first.get("load_source")] += 1
            total += 1
    shares = {h: round(c / total, 3) for h, c in by_host.items()} if total else {}
    out = {"selections": total, "by_host": dict(by_host), "shares": shares, "by_instance": dict(by_instance),
           "skipped_reasons": dict(skipped), "load_sources": dict(sources),
           "score_order": dict(gaps), "inputs_sample": inputs[:: max(1, len(inputs) // 12)][:12]}
    ok = True
    if a.max_share is not None:
        ok = total > 0 and len(by_host) >= 2 and max(shares.values()) <= a.max_share
    if a.min_hosts:
        ok = ok and len(by_host) >= a.min_hosts
    out["ok"] = ok
    print(json.dumps(out))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
