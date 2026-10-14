#!/usr/bin/env python3
"""CapyCTL's load read of one deployment while a request runs: polls
`GET /management/v1/metrics/load` (docs/guide/parking.md#reading-load) with the
admin token every half second until process PID exits (at most LIMIT_S
seconds), then prints one line: `pass|fail<TAB>detail`. Every read is appended
to OUT as JSON lines.

CapyCTL samples the engine's loopback `/metrics` with the launch's own key
(SPEC §10, ADR 0013 §10). Under deep parking a vLLM launch runs behind
CapyCTL's key guard, which keys `/metrics` (runtime/capyctl_vllm_guard.py), so
an unkeyed curl of the engine says nothing about whether CapyCTL can read it;
this read does. Pass: at least one read held a fresh sample for the deployment
with the engine's running and waiting counts present (not null).

usage: load_check.py CREDENTIALS_FILE MANAGEMENT_ADDRESS_FILE DEPLOYMENT PID OUT [LIMIT_S]
"""
import json, os, sys, time, urllib.request

cred, address_file, name, pid, out = sys.argv[1:6]
limit = float(sys.argv[6]) if len(sys.argv) > 6 else 30.0
token = next(l.split(":", 1)[1].strip() for l in open(cred) if l.startswith("admin_token:"))
address = open(address_file).read().strip()


def alive(p):
    """PID still runs. The shell reaps its background request only at `wait`,
    so a finished one lingers as a zombie: that counts as exited."""
    try:
        with open(f"/proc/{int(p)}/stat") as stat:
            return stat.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False


reads = fresh = 0
peak = None
errors = []
deadline = time.monotonic() + limit
with open(out, "a") as log:
    while True:
        time.sleep(0.5)
        try:
            req = urllib.request.Request(f"http://{address}/management/v1/metrics/load",
                                         headers={"Authorization": f"Bearer {token}"})
            with urllib.request.urlopen(req, timeout=5) as resp:
                body = json.load(resp)
        except Exception as e:  # recorded, never fatal: the next read may succeed
            errors.append(type(e).__name__)
            body = None
        if body is not None:
            reads += 1
            for dep in body.get("deployments", []):
                if dep.get("name") != name:
                    continue
                for inst in dep.get("instances", []):
                    sample = inst.get("sample") or {}
                    engine = sample.get("engine") or {}
                    log.write(json.dumps({"t": round(time.time(), 3), "instance": inst.get("index"),
                                          "router_in_flight": inst.get("router_in_flight"),
                                          "sample": inst.get("sample")}) + "\n")
                    if sample.get("fresh") and engine.get("running") is not None \
                            and engine.get("waiting") is not None:
                        fresh += 1
                        peak = max(peak or 0, engine["running"])
        if not alive(pid) or time.monotonic() > deadline:
            break

detail = f"{reads} load reads, {fresh} with a fresh engine sample"
if peak is not None:
    detail += f", engine running up to {peak:g}"
if errors:
    detail += f", {len(errors)} failed reads ({errors[-1]})"
print(("pass" if fresh else "fail") + "\t" + detail)
