#!/usr/bin/env python3
"""M48 soak: a seeded random walk over the two-host control plane (plan unit W2).

    soak.py --plan PLAN.json --steps 200 --seed N [--from-step K] [--max-hours H]

rows/M48.sh writes the plan (deployments, fixture files, host policies) and runs
this with the run's settings in the environment (RUN, LRD, SERVER_CFG, SERVER_DB,
MLLM, EVID, RUNSTATE, RRD, REMOTE_TREE, HOST_ID_92, HOST_ID_17, MLLM_API_KEY).

Each step picks one operation with a random generator derived from (seed, step),
so a step's choice is reproducible given the observed state, and a walk that
stopped can resume with --from-step. Operations (matrix M48, 2026-09-24 brief):
routed inference (streamed or not), tool calls, operator start (with or without
--evict), stop, park, wake on request, request-driven switching on the tight
host, instance count change, stop and start of one instance, delete --stop and
redeploy, drain host, host agent SIGTERM and restart, SIGKILL of an owned engine
process, and a short SIGSTOP/SIGCONT of a host agent.

After every step the walk waits for the deployments to settle and then checks
the invariants, each read-only (CLI status, SELECTs on the server database,
/proc and nvidia-smi on each Spark):

  I-LEDGER   per host, the charged bytes are within the host's managed_limit and
             the parked count within max_parked
  I-LEASE    no request lease is held when no request is in flight (an uncertain
             lease is tracked against the deadline, I-UNCERTAIN)
  I-I1       every deployment with a Ready instance answers the model's I1 golden,
             and every deployment of a model recorded the same checkpoint digest
  I-ORPHAN   every engine process on a host belongs to a live or uncertain binding
             (the recorded identity or one of its descendants)
  I-GPU      every GPU compute process is an owned process
  I-STATE    observed states agree with the ledger: Ready and Parked instances hold
             a charge in that phase, a binding and live identities; stopped ones
             hold nothing; no charge, binding or endpoint lease belongs to nothing
  I-DEAD     identities an operation ended (stop, drain, delete, kill) are gone
  I-SAME     identities an operation must keep (park, wake, agent restart, freeze)
             are the same processes afterwards
  I-UNCERTAIN no uncertain binding or lease, failed or transitional instance, or
             lifecycle claim outlives the deadline without a journaled reason

Signals go only to owned identities (fault.sh, signal_owned.py). Nothing here
writes the database or reads an engine key. Evidence: EVID/soak/steps.jsonl (one
record per step), EVID/soak/violations.jsonl, EVID/soak/summary.json and
per-step detail under EVID/soak/steps/. Running this is not a pass of M48; the
row is judged from the evidence. CPU and Fake-engine tests are not evidence.
"""

import argparse
import json
import os
import random
import re
import sqlite3
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import matrixhttp  # noqa: E402

ENV = os.environ
MLLM = ENV.get("MLLM", "")
SERVER_CFG = ENV.get("SERVER_CFG", "")
SERVER_DB = ENV.get("SERVER_DB", "")
EVID = ENV.get("EVID", "")
RRD = ENV.get("RRD", "")
REMOTE_TREE = ENV.get("REMOTE_TREE", "")
RUNSTATE = ENV.get("RUNSTATE", "")
HOSTS = ("host-a", "host-b")
ENGINE_RE = r"sglang[.]launch_server|sglang_entr[y]|vllm_entr[y]|vllm[ ]serve|Engine[C]ore|sglang::schedule[r]|sglang::detokenize[r]"
STABLE = {"ready", "parked", "stopped", "failed"}
# I1 compares the first I1_PREFIX greedy tokens. The shakedown (2026-09-24) found
# vLLM q4 flipping a near tie at token 10 (logprob margin 0.05) once the probe
# prompt sat in the prefix cache: a numerical path change, not another model.
# The model goldens differ from token 0, so eight tokens still tell them apart.
I1_PREFIX = int(ENV.get("SOAK_I1_PREFIX", "8"))
UNCERTAIN_DEADLINE_S =UNCERTAIN_DEADLINE_S = float(ENV.get("SOAK_UNCERTAIN_DEADLINE_S", "900"))
SETTLE_S = float(ENV.get("SOAK_SETTLE_S", "900"))
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


class SshLost(Exception):
    """SSH to a Spark failed twice in a row: stop the walk cleanly."""


def now_ms():
    return int(time.time() * 1000)


def ulid_ms(ulid):
    value = 0
    for ch in ulid[:10]:
        value = value * 32 + CROCKFORD.index(ch)
    return value


def host_id(host):
    return ENV["HOST_ID_92"] if host == "host-a" else ENV["HOST_ID_17"]


def host_name(hid):
    return {ENV["HOST_ID_92"]: "host-a", ENV["HOST_ID_17"]: "host-b"}.get(hid, hid)


# --- local tools ---------------------------------------------------------------

def cli(*args, timeout=1800):
    """One CLI call against the run's server; returns (rc, parsed-or-text, stderr)."""
    cmd = [MLLM, *args, "--config", SERVER_CFG]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 124, None, f"timeout after {timeout}s"
    out = proc.stdout.strip()
    try:
        parsed = json.loads(out) if out else None
    except ValueError:
        parsed = out
    return proc.returncode, parsed, proc.stderr.strip()[-2000:]


def script(path, *args, timeout=900):
    try:
        proc = subprocess.run([os.path.join(HERE, path), *args], capture_output=True, text=True, timeout=timeout)
        return proc.returncode, proc.stdout[-4000:], proc.stderr[-4000:]
    except subprocess.TimeoutExpired:
        return 124, "", f"timeout after {timeout}s"


def db_rows(sql, params=()):
    assert sql.lstrip().upper().startswith("SELECT")
    db = sqlite3.connect(f"file:{SERVER_DB}?mode=ro", uri=True, timeout=10)
    try:
        db.execute("PRAGMA query_only=1")
        db.row_factory = sqlite3.Row
        return [dict(r) for r in db.execute(sql, params)]
    finally:
        db.close()


def ledger():
    """Read-only accounting snapshot (the same SELECTs as ledger.py snapshot)."""
    out = {
        "deployments": db_rows("SELECT id,name,desired_state,observed_state,current_generation,revision,admission_enabled FROM deployments"),
        "reservations": db_rows("SELECT owner_id,domain_id,bytes,phase FROM reservations"),
        "resource_owners": db_rows("SELECT owner_id,footprint_json FROM resource_owners"),
        "endpoint_leases": db_rows("SELECT host_id,host,port,binding_id FROM endpoint_leases"),
        "lifecycle_claims": db_rows("SELECT deployment_id,operation_id,revision,generation FROM lifecycle_claims"),
        "request_leases": db_rows("SELECT id,deployment_id,revision,generation,disposition FROM request_leases"),
        "runtime_bindings": db_rows(
            "SELECT b.id,b.deployment_id,b.revision,b.state,b.identities_json,i.host_id FROM runtime_bindings b "
            "LEFT JOIN remote_binding_ingress i ON i.binding_id=b.id WHERE b.state!='released'"),
    }
    for b in out["runtime_bindings"]:
        b["identities"] = json.loads(b.pop("identities_json") or "[]")
    for o in out["resource_owners"]:
        o["footprint"] = json.loads(o.pop("footprint_json"))
    return out


def journal_reason(deployment_id, since_ms):
    """A journal entry recorded for the deployment since the item appeared (read-only)."""
    rows = db_rows("SELECT j.state,j.recorded_at FROM journal_entries j JOIN operations o ON o.id=j.operation_id "
                   "WHERE o.deployment_id=? ORDER BY j.recorded_at DESC LIMIT 5", (deployment_id,))
    return rows


# --- remote probe --------------------------------------------------------------

PROBE = r'''
import glob, json, os, re, subprocess, sys
req = json.loads(sys.argv[1])
boot = open("/proc/sys/kernel/random/boot_id").read().strip()
def stat(pid):
    try:
        s = open(f"/proc/{pid}/stat").read()
        f = s.rsplit(")", 1)[1].split()
        cmd = open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\0", b" ").decode(errors="replace").strip()
        return {"pid": pid, "state": f[0], "ppid": int(f[1]), "pgid": int(f[2]), "ticks": int(f[19]), "cmd": cmd[:160]}
    except (OSError, IndexError, ValueError):
        return None
procs = {}
for d in os.listdir("/proc"):
    if d.isdigit():
        p = stat(int(d))
        if p:
            procs[p["pid"]] = p
children = {}
for p in procs.values():
    children.setdefault(p["ppid"], []).append(p["pid"])
def desc(pid):
    out, stack = set(), [pid]
    while stack:
        for c in children.get(stack.pop(), []):
            if c not in out:
                out.add(c); stack.append(c)
    return out
owned, idents = set(), []
for i in req["identities"]:
    p = procs.get(i["pid"])
    alive = p is not None and p["ticks"] == i["ticks"] and i["boot"] == boot
    idents.append({**i, "alive": alive, "pstate": p["state"] if alive else None})
    if alive and i.get("owning", True):
        owned.add(i["pid"]); owned |= desc(i["pid"])
agent = None
try:
    pid, ticks, _ = open(req["agent_pidfile"]).read().split()[:3]
    p = procs.get(int(pid))
    agent = {"pid": int(pid), "alive": p is not None and p["ticks"] == int(ticks), "pstate": p and p["state"]}
except Exception as e:
    agent = {"error": str(e)}
pat = re.compile(req["engine_re"])
me = os.getpid()
engine = []
for p in procs.values():
    if p["pid"] == me or p["ppid"] == me:
        continue
    if pat.search(p["cmd"]):
        engine.append(p)
# Python processes below the agent are engine processes too (a launch in flight).
if agent and agent.get("alive"):
    for pid in desc(agent["pid"]):
        p = procs.get(pid)
        if p and p not in engine and re.search(r"python|vllm|sglang", p["cmd"]):
            engine.append(p)
orphans = [p for p in engine if p["pid"] not in owned]
gpu = []
try:
    out = subprocess.run(["nvidia-smi", "--query-compute-apps=pid,used_memory", "--format=csv,noheader"],
                         capture_output=True, text=True, timeout=30).stdout
    for line in out.splitlines():
        if line.strip():
            pid, mem = [x.strip() for x in line.split(",", 1)]
            gpu.append({"pid": int(pid), "mem": mem, "owned": int(pid) in owned,
                        "cmd": (procs.get(int(pid)) or {}).get("cmd")})
except Exception as e:
    gpu = [{"error": str(e)}]
mem = next((int(l.split()[1]) for l in open("/proc/meminfo") if l.startswith("MemAvailable")), None)
pyc = [p for p in glob.glob(req["runtime"] + "/**/__pycache__", recursive=True)] + \
      [p for p in glob.glob(req["runtime"] + "/**/*.pyc", recursive=True)]
print(json.dumps({"boot": boot, "identities": idents, "agent": agent, "engine_procs": len(engine),
                  "orphans": orphans, "gpu": gpu, "mem_available_kb": mem,
                  "rendezvous": glob.glob("/tmp/mllm-rdzv-*") + glob.glob(req["rrd"] + "/host/rendezvous/*"),
                  "bytecode": pyc[:10]}))
'''


def ssh(host, argv, stdin=None, timeout=120):
    cmd = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", host, *argv]
    for attempt in range(2):
        try:
            proc = subprocess.run(cmd, input=stdin, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            if attempt:
                raise SshLost(f"{host}: ssh timed out twice")
            continue
        if proc.returncode != 255:
            return proc
        if attempt:
            raise SshLost(f"{host}: ssh failed twice: {proc.stderr.strip()[-300:]}")
        time.sleep(10)
    raise SshLost(host)


def probe_host(host, identities):
    req = {"identities": identities, "agent_pidfile": f"{RRD}/host.pid", "engine_re": ENGINE_RE,
           "runtime": f"{REMOTE_TREE}/runtime", "rrd": RRD}
    # The request travels inside the script on stdin, never through the remote shell.
    source = f"import sys\nsys.argv = ['probe', {json.dumps(json.dumps(req))}]\n" + PROBE
    proc = ssh(host, ["python3", "-B", "-"], stdin=source)
    try:
        return json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {"error": (proc.stdout + proc.stderr)[-600:]}


# --- the walk ------------------------------------------------------------------

class Soak:
    def __init__(self, plan, seed, out):
        self.plan = plan
        self.deps = plan["deployments"]
        self.seed = seed
        self.out = out
        os.makedirs(os.path.join(out, "steps"), exist_ok=True)
        self.first_seen = {}   # uncertain item key -> first seen ms
        self.persistent = {}   # violation key -> first step
        self.counts = {}
        self.violation_total = 0
        self.expect_dead = []  # identities an operation ended
        self.expect_same = []  # identities an operation must keep
        self.limits = plan["limits"]
        self.inflight = 0

    # observed state
    def status(self, name):
        rc, data, _ = cli("status", "deployment", name, "--output", "json", timeout=60)
        if rc != 0 or not isinstance(data, dict):
            return None
        return data.get("deployment", data)

    def statuses(self):
        return {n: self.status(n) for n in self.deps}

    def weight(self, name):
        return 3 if self.deps[name]["model"] == "4" else 1

    def pick(self, rng, names):
        names = sorted(names)
        if not names:
            return None
        return rng.choices(names, weights=[self.weight(n) for n in names])[0]

    @staticmethod
    def inst_states(st):
        return [(i.get("index"), i.get("observed_state"), i.get("host_id")) for i in (st or {}).get("instances", [])]

    def ready(self, st):
        return [i for i in (st or {}).get("instances", []) if i.get("observed_state") == "ready"]

    def charged_on(self, led, hid):
        total, parked = 0, 0
        for o in led["resource_owners"]:
            fp = o["footprint"]
            mine = [a for a in fp.get("allocations", []) if f":{hid}/" in a[0]]
            total += sum(a[1] for a in mine)
            if mine and fp.get("phase") == "parked":
                parked += 1
        return total, parked

    def request(self, name, stream=False, timeout=1800):
        a, b = random.Random(now_ms()).randint(10, 49), random.Random(now_ms() + 7).randint(10, 49)
        prompt = f"What is {a}+{b}? Answer with only the number."
        self.inflight += 1
        try:
            rec = matrixhttp.chat(self.deps[name]["route"], [{"role": "user", "content": prompt}], stream=stream,
                                  max_tokens=1024, timeout=timeout)
        finally:
            self.inflight -= 1
        rec["prompt"] = prompt
        rec["expect"] = str(a + b)
        ok = rec.get("status") == 200 and str(a + b) in (rec.get("content") or "") and (
            not stream or rec.get("sse_well_formed"))
        rec["verdict"] = "ok" if ok else "failed"
        # An operator stop is sticky: a request never reactivates it (closed 429).
        if rec.get("status") == 429 and "explicitly stopped" in json.dumps(rec.get("error") or ""):
            rec["verdict"] = "refused"
        matrixhttp.append_jsonl(os.path.join(self.out, "requests.jsonl"), rec)
        return ok, {k: rec.get(k) for k in ("status", "content", "elapsed_s", "verdict", "error", "transport_error",
                                            "sse_well_formed", "finish_reason", "marker")}

    def identities_of(self, led, dep_id, hid=None):
        out = []
        for b in led["runtime_bindings"]:
            if b["deployment_id"] == dep_id and (hid is None or b["host_id"] == hid):
                for i in b["identities"]:
                    out.append({"pid": i["pid"], "ticks": i["start_ticks"], "boot": i["boot_id"], "role": i.get("role"),
                                "host": host_name(b["host_id"]), "binding": b["id"], "deployment": dep_id})
        return out

    # operations: each returns (outcome, detail); outcome in ok, refused, failed, skipped
    def op_infer(self, rng, sts, led, stream=False):
        name = self.pick(rng, [n for n in self.deps if sts.get(n)])
        if not name:
            return "skipped", {"why": "no deployment"}
        before = self.inst_states(sts[name])
        kind = "infer" if self.ready(sts[name]) else "activate-on-request"
        ok, rec = self.request(name, stream=stream)
        return ("ok" if ok else rec["verdict"]), {"target": name, "kind": kind, "before": before, "request": rec}

    def op_stream(self, rng, sts, led):
        return self.op_infer(rng, sts, led, stream=True)

    def op_toolcall(self, rng, sts, led):
        name = self.pick(rng, [n for n in self.deps if sts.get(n)])
        if not name:
            return "skipped", {"why": "no deployment"}
        choice = rng.choice(["named", "auto"])
        stream = rng.random() < 0.5
        args = ["--route", self.deps[name]["route"], "--choice", choice, "--out", os.path.join(self.out, "toolcall.jsonl")]
        if stream:
            args.append("--stream")
        self.inflight += 1
        try:
            proc = subprocess.run([sys.executable, os.path.join(HERE, "toolcall.py"), *args], capture_output=True,
                                  text=True, timeout=1800)
        except subprocess.TimeoutExpired:
            self.inflight -= 1
            return "failed", {"target": name, "why": "toolcall timed out"}
        self.inflight -= 1
        try:
            rec = json.loads(proc.stdout.strip().splitlines()[-1])
        except (ValueError, IndexError):
            rec = {"raw": proc.stdout[-500:], "err": proc.stderr[-500:]}
        if rec.get("status") == 429 and "explicitly stopped" in json.dumps(rec.get("error") or ""):
            return "refused", {"target": name, "choice": choice, "stream": stream, "error": rec.get("error")}
        detail = {"target": name, "choice": choice, "stream": stream,
                  "status": rec.get("status"), "verdict": rec.get("verdict"),
                  "tool_calls": rec.get("tool_calls"), "finish_reason": rec.get("finish_reason"), "error": rec.get("error")}
        return ("ok" if proc.returncode == 0 else "failed"), detail

    def op_start(self, rng, sts, led):
        cands = [n for n, s in sts.items() if s and not self.ready(s)]
        name = self.pick(rng, cands)
        if not name:
            return "skipped", {"why": "every deployment has a ready instance"}
        evict = "host-b" in self.deps[name]["hosts"] and rng.random() < 0.5
        args = ["start", "deployment", name, "--wait", "--output", "json"] + (["--evict"] if evict else [])
        rc, data, err = cli(*args)
        return ("ok" if rc == 0 else "refused"), {"target": name, "evict": evict, "rc": rc, "out": data, "err": err}

    def op_stop(self, rng, sts, led):
        cands = [n for n, s in sts.items() if s and any(st != "stopped" for _, st, _ in self.inst_states(s))]
        name = self.pick(rng, cands)
        if not name:
            return "skipped", {"why": "nothing running"}
        dead = self.identities_of(led, sts[name]["id"])
        rc, data, err = cli("stop", "deployment", name, "--output", "json")
        waited = self.wait_until(name, lambda s: all(st == "stopped" for _, st, _ in self.inst_states(s)), 900)
        if rc == 0 and waited:
            self.expect_dead += dead
        return ("ok" if rc == 0 and waited else "failed" if rc == 0 else "refused"), {
            "target": name, "rc": rc, "out": data, "err": err, "stopped": waited, "identities": len(dead)}

    def op_park(self, rng, sts, led):
        cands = [n for n, s in sts.items() if s and self.ready(s)]
        name = self.pick(rng, cands)
        if not name:
            return "skipped", {"why": "nothing ready"}
        keep = self.identities_of(led, sts[name]["id"])
        rc, data, err = cli("park", "deployment", name, "--output", "json")
        if rc != 0:
            return "refused", {"target": name, "rc": rc, "out": data, "err": err}
        parked = self.wait_until(name, lambda s: not self.ready(s) and all(
            st in STABLE for _, st, _ in self.inst_states(s)), 900)
        s = self.status(name)
        states = self.inst_states(s)
        if all(st == "parked" for _, st, _ in states if st != "stopped") and any(st == "parked" for _, st, _ in states):
            self.expect_same += keep
        return ("ok" if parked else "failed"), {"target": name, "rc": rc, "out": data, "after": states}

    def op_wake(self, rng, sts, led):
        cands = [n for n, s in sts.items() if s and any(st == "parked" for _, st, _ in self.inst_states(s))]
        name = self.pick(rng, cands)
        if not name:
            return "skipped", {"why": "nothing parked"}
        parked_hosts = {h for _, st, h in self.inst_states(sts[name]) if st == "parked"}
        keep = [i for h in parked_hosts for i in self.identities_of(led, sts[name]["id"], h)]
        ok, rec = self.request(name, stream=rng.random() < 0.3)
        after = self.inst_states(self.status(name))
        # A wake keeps its processes (warm); a parked sibling that stayed parked keeps them too.
        now = {(i["pid"], i["ticks"]) for i in self.identities_of(ledger(), sts[name]["id"])}
        warm = all((i["pid"], i["ticks"]) in now for i in keep)
        self.expect_same += keep
        return ("ok" if ok else "failed"), {"target": name, "request": rec, "after": after, "warm": warm,
                                            "kept": len(keep)}

    def op_switch(self, rng, sts, led):
        """Request-driven switching on the tight host: its single-instance
        deployments fit two at a time, so with two Ready a request for the third
        must park or stop one of them (M27/M31). Incumbents are started first when
        needed; when every candidate target is operator-stopped the switch is the
        operator's `start --evict` instead."""
        hid = host_id("host-b")
        trio = sorted(n for n, d in self.deps.items() if d["hosts"] == ["host-b"] and sts.get(n))
        if len(trio) < 3:
            return "skipped", {"why": "fewer than three single-instance deployments on host-b"}

        def op_stopped(n):
            s = sts[n] or {}
            return s.get("desired_state") == "stopped" or any(i.get("operator_stopped") for i in s.get("instances", []))

        idle = [n for n in trio if not self.ready(sts[n])]
        if not idle:
            idle = trio
        free = [n for n in idle if not op_stopped(n)]
        target = self.pick(rng, free or idle)
        prepared = []
        for n in trio:
            if n != target and not self.ready(self.status(n) or {}):
                rc, data, err = cli("start", "deployment", n, "--wait", "--output", "json")
                prepared.append({"start": n, "rc": rc, "err": err[-300:]})
        led = ledger()
        charged, _ = self.charged_on(led, hid)
        limit = self.limits["host-b"]["managed_limit"]
        target_st = self.status(target) or {}
        needs = not self.ready(target_st) and charged + self.deps[target]["request_bytes"] > limit
        before = {n: self.inst_states(self.status(n)) for n in self.deps}
        if self.ready(target_st):
            # Every one was already up (the replica held no host-b charge): park one to request back.
            return "skipped", {"why": "target already ready", "prepared": prepared}
        if op_stopped(target):
            rc, data, err = cli("start", "deployment", target, "--evict", "--wait", "--output", "json")
            ok, rec = rc == 0, {"rc": rc, "out": data, "err": err[-600:]}
            kind = "operator-evict"
        else:
            ok, rec = self.request(target, stream=rng.random() < 0.3)
            kind = "request"
        after = {n: self.inst_states(self.status(n)) for n in self.deps}
        victims = [n for n in trio if n != target and any(st == "ready" for _, st, _ in before[n])
                   and not any(st == "ready" for _, st, _ in after[n])]
        return ("ok" if ok else "failed"), {"target": target, "kind": kind, "needed_switch": needs,
                                            "charged_before": charged, "limit": limit, "prepared": prepared,
                                            "victims": victims, "request": rec, "before": before, "after": after}

    def op_count(self, rng, sts, led):
        rep = self.plan.get("replica")
        s = sts.get(rep) if rep else None
        if not s:
            return "skipped", {"why": "no replica deployment"}
        want = 1 if int(s.get("desired_instances", 2)) == 2 else 2
        path = self.deps[rep]["file_one"] if want == 1 else self.deps[rep]["file"]
        rc, data, err = cli("deploy", "model", "--file", path, "--revision", str(s.get("revision")), "--output", "json")
        after = self.wait_until(rep, lambda x: int(x.get("desired_instances", 0)) == want, 300)
        return ("ok" if rc == 0 and after else "refused" if rc else "failed"), {
            "target": rep, "to": want, "rc": rc, "out": data, "err": err, "states": self.inst_states(self.status(rep))}

    def op_instance(self, rng, sts, led):
        rep = self.plan.get("replica")
        s = sts.get(rep) if rep else None
        if not s or not s.get("instances"):
            return "skipped", {"why": "no replica instances"}
        inst = rng.choice(s["instances"])
        idx, state, hid = inst.get("index"), inst.get("observed_state"), inst.get("host_id")
        target = f"{rep}/{idx}"
        if state in ("ready", "parked"):
            dead = self.identities_of(led, s["id"], hid)
            rc, data, err = cli("stop", "instance", target, "--output", "json")
            # A count-only revision keeps a running instance on its old index; once
            # it stops, that index is retired and status lists index 0 in its place
            # (ADR 0013 section 7; found live in the first soak, step 54). So the
            # stop is done when that index is stopped or gone.
            done = self.wait_until(rep, lambda x: not any(i.get("index") == idx and i.get("observed_state") != "stopped"
                                                          for i in x.get("instances", [])), 900)
            if rc == 0 and done:
                self.expect_dead += dead
            return ("ok" if rc == 0 and done else "refused" if rc else "failed"), {
                "target": target, "action": "stop", "rc": rc, "out": data, "err": err}
        evict = host_name(hid) == "host-b" and rng.random() < 0.5
        rc, data, err = cli("start", "instance", target, "--wait", "--output", "json", *(["--evict"] if evict else []))
        return ("ok" if rc == 0 else "refused"), {"target": target, "action": "start", "evict": evict, "from": state,
                                                  "rc": rc, "out": data, "err": err}

    def op_delete_redeploy(self, rng, sts, led):
        name = self.pick(rng, [n for n in self.deps if sts.get(n)])
        if not name:
            return "skipped", {"why": "no deployment"}
        dep_id = sts[name]["id"]
        dead = self.identities_of(led, dep_id)
        rc, data, err = cli("delete", "deployment", name, "--stop", "--output", "json", timeout=1800)
        detail = {"target": name, "deployment_id": dep_id, "delete_rc": rc, "delete_out": data, "delete_err": err}
        if rc != 0 or not (isinstance(data, dict) and data.get("deleted")):
            return "failed", detail
        self.expect_dead += dead
        residue = self.residue(dep_id)
        detail["residue"] = residue
        activate = self.deps[name]["instances"] == 1 and rng.random() < 0.5
        args = ["deploy", "model", "--file", self.deps[name]["file"], "--output", "json"]
        if activate:
            args += ["--activate", "--wait"]
        rc2, data2, err2 = cli(*args)
        detail.update({"redeploy_rc": rc2, "redeploy_out": data2, "redeploy_err": err2, "activate": activate})
        if residue:
            return "failed", detail
        if rc2 != 0 and self.status(name) is None:
            return "failed", detail
        return ("ok" if rc2 == 0 else "refused"), detail

    def residue(self, dep_id):
        led = ledger()
        hits = {}
        for key, rows in led.items():
            found = [r for r in rows if dep_id in json.dumps(r)
                     and not (key == "deployments" and r.get("name") == f"deleted/{dep_id}")]
            if found:
                hits[key] = found
        return hits

    def op_drain(self, rng, sts, led):
        host = rng.choice(HOSTS)
        hid = host_id(host)
        dead = [i for n, s in sts.items() if s for i in self.identities_of(led, s["id"], hid)]
        rc, data, err = cli("drain", "host", host, "--wait", "--output", "json", timeout=1800)
        if rc == 0:
            self.expect_dead += dead
        return ("ok" if rc == 0 else "failed"), {"target": host, "rc": rc, "out": data, "err": err, "identities": len(dead)}

    def op_agent_restart(self, rng, sts, led):
        host = rng.choice(HOSTS)
        hid = host_id(host)
        keep = [i for n, s in sts.items() if s for i in self.identities_of(led, s["id"], hid)]
        ready_before = {n: [i.get("index") for i in s.get("instances", []) if i.get("host_id") == hid
                            and i.get("observed_state") == "ready"] for n, s in sts.items() if s}
        steps = []
        for args in (("host-down", host, "TERM"), ("host-up", host), ("wait-online", "180")):
            rc, out, err = script("roles.sh", *args, timeout=600)
            steps.append({"args": args, "rc": rc, "err": err[-600:]})
            if rc != 0:
                return "failed", {"target": host, "steps": steps}
        back = True
        for n, idxs in ready_before.items():
            if idxs:
                back &= self.wait_until(n, lambda s, idxs=idxs: all(any(
                    i.get("index") == x and i.get("observed_state") == "ready" for i in s.get("instances", []))
                    for x in idxs), 300)
        self.expect_same += keep
        return ("ok" if back else "failed"), {"target": host, "steps": steps, "ready_again": back,
                                              "ready_before": ready_before}

    def op_engine_kill(self, rng, sts, led):
        cands = []
        for n, s in sts.items():
            for i in self.ready(s):
                cands.append((n, host_name(i.get("host_id")), i.get("index")))
        if not cands:
            return "skipped", {"why": "nothing ready"}
        cands = sorted(cands)
        name, host, idx = rng.choices(cands, weights=[self.weight(c[0]) for c in cands])[0]
        dead = self.identities_of(led, sts[name]["id"], host_id(host))
        rc, out, err = script("fault.sh", "engine", host, name, "KILL", "api", timeout=120)
        if rc != 0:
            return "failed", {"target": name, "host": host, "rc": rc, "out": out, "err": err}
        left = self.wait_until(name, lambda s: any(i.get("index") == idx and i.get("observed_state") in (
            "failed", "stopped") for i in s.get("instances", [])), 300)
        # The charge stays until gone evidence; then it is released (M36).
        released = self.wait_until(name, lambda s: self.charge_gone(sts[name]["id"], host_id(host)), 300)
        self.expect_dead += dead
        return ("ok" if left and released else "failed"), {
            "target": name, "host": host, "index": idx, "signal": out.strip()[-300:], "left_ready": left,
            "charge_released": released, "after": self.inst_states(self.status(name))}

    def charge_gone(self, dep_id, hid):
        led = ledger()
        return not any(o["owner_id"] == dep_id or o["owner_id"].startswith(f"deployment:{dep_id}/")
                       for o in led["resource_owners"]
                       if any(f":{hid}/" in a[0] for a in o["footprint"].get("allocations", [])))

    def op_freeze(self, rng, sts, led):
        host = rng.choice(HOSTS)
        hid = host_id(host)
        keep = [i for n, s in sts.items() if s for i in self.identities_of(led, s["id"], hid)]
        seconds = rng.randint(4, 8)
        target = self.pick(rng, [n for n, s in sts.items() if s and any(
            i.get("host_id") == hid for i in self.ready(s)) and self.deps[n]["instances"] == 1])
        result = {}

        def bg():
            ok, rec = self.request(target, timeout=900)
            result.update({"ok": ok, "request": rec})

        thread = None
        rc, out, err = script("fault.sh", "agent", host, "STOP", timeout=120)
        if rc != 0:
            return "failed", {"target": host, "stop": err}
        try:
            if target:
                thread = threading.Thread(target=bg)
                thread.start()
            time.sleep(seconds)
        finally:
            rc2, out2, err2 = script("fault.sh", "agent", host, "CONT", timeout=120)
        if thread:
            thread.join(timeout=960)
        rc3, _, err3 = script("roles.sh", "wait-online", "180", timeout=300)
        self.expect_same += keep
        # A request during the freeze is served, or refused with a closed retryable code.
        req = result.get("request") or {}
        closed = req.get("status") in (503, 429) or result.get("ok")
        outcome = "ok" if rc2 == 0 and rc3 == 0 and (not target or closed) else "failed"
        return outcome, {"target": host, "seconds": seconds, "request_target": target, "result": result,
                         "cont_rc": rc2, "online_rc": rc3, "online_err": err3[-300:]}

    OPS = [
        ("infer", 14), ("stream", 10), ("toolcall", 5), ("start", 6), ("stop", 7), ("park", 7),
        ("wake", 6), ("switch", 8), ("count", 3), ("instance", 5), ("delete_redeploy", 3),
        ("drain", 2), ("agent_restart", 2), ("engine_kill", 3), ("freeze", 2),
    ]

    def wait_until(self, name, pred, timeout):
        end = time.time() + timeout
        while time.time() < end:
            s = self.status(name)
            if s is not None and pred(s):
                return True
            time.sleep(2)
        return False

    def settle(self):
        """Wait until every instance is in a stable state; return the transitional ones left."""
        end = time.time() + SETTLE_S
        while True:
            sts = self.statuses()
            moving = {n: [(i, st) for i, st, _ in self.inst_states(s) if st not in STABLE]
                      for n, s in sts.items() if s}
            moving = {n: v for n, v in moving.items() if v}
            if not moving or time.time() > end:
                return sts, moving
            time.sleep(3)

    # invariants
    def check(self, step, sts, moving):
        led = ledger()
        v = []
        info = {"moving": moving}
        dep_by_id = {s["id"]: (n, s) for n, s in sts.items() if s}
        # I-LEDGER
        per_host = {}
        for host in HOSTS:
            hid = host_id(host)
            charged, parked = self.charged_on(led, hid)
            lim = self.limits[host]
            per_host[host] = {"charged": charged, "parked": parked, **lim}
            if charged > lim["managed_limit"]:
                v.append(("I-LEDGER", f"{host} charged {charged} > managed_limit {lim['managed_limit']}"))
            if parked > lim["max_parked"]:
                v.append(("I-LEDGER", f"{host} parked {parked} > max_parked {lim['max_parked']}"))
        info["ledger"] = per_host
        info["reservations"] = led["reservations"]
        # I-LEASE and I-UNCERTAIN for leases
        for lease in led["request_leases"]:
            key = f"lease:{lease['id']}"
            age = (now_ms() - ulid_ms(lease["id"])) / 1000
            if lease["disposition"] == "inflight" and self.inflight == 0:
                v.append(("I-LEASE", f"in-flight lease {lease['id']} for {lease['deployment_id']} with no request in flight (age {age:.0f}s)"))
            elif lease["disposition"] == "uncertain":
                self.track(key, v, age, lease["deployment_id"], f"uncertain lease {lease['id']}")
        info["leases"] = led["request_leases"]
        # bindings, charges and endpoint leases belong to something
        bindings = {(b["deployment_id"], b["host_id"]): b for b in led["runtime_bindings"]}
        owners = {o["owner_id"]: o for o in led["resource_owners"]}
        for b in led["runtime_bindings"]:
            if b["deployment_id"] not in dep_by_id:
                v.append(("I-STATE", f"binding {b['id']} ({b['state']}) belongs to no current deployment {b['deployment_id']}"))
            if b["state"] == "uncertain":
                self.track(f"binding:{b['id']}", v, None, b["deployment_id"], f"uncertain binding {b['id']}")
        for o in led["resource_owners"]:
            dep_id = o["owner_id"].split("/")[0].replace("deployment:", "")
            if dep_id not in dep_by_id:
                v.append(("I-STATE", f"charge {o['owner_id']} belongs to no current deployment"))
        live_binding_ids = {b["id"] for b in led["runtime_bindings"]}
        for lease in led["endpoint_leases"]:
            if lease["binding_id"] not in live_binding_ids:
                v.append(("I-STATE", f"endpoint lease {lease['host']}:{lease['port']} for released binding {lease['binding_id']}"))
        for claim in led["lifecycle_claims"]:
            self.track(f"claim:{claim['operation_id']}", v, None, claim["deployment_id"], f"lifecycle claim {claim['operation_id']}")
        # I-STATE per instance
        seen_bindings, seen_owners = set(), set()
        for n, s in sts.items():
            if not s:
                continue
            # A stopped instance keeps its last host, and a sibling may since have
            # been placed there (live, step 41 of the first soak): a binding on that
            # host belongs to the instance that is not stopped.
            active_hosts = {i.get("host_id") for i in s.get("instances", []) if i.get("observed_state") != "stopped"}
            for i in s.get("instances", []):
                st, hid, owner = i.get("observed_state"), i.get("host_id"), i.get("reservation_owner")
                b = bindings.get((s["id"], hid))
                if st == "stopped" and hid in active_hosts:
                    b = None
                o = owners.get(owner)
                tag = f"{n}/{i.get('index')}@{host_name(hid)}"
                if b:
                    seen_bindings.add(b["id"])
                if o:
                    seen_owners.add(owner)
                phase = o["footprint"].get("phase") if o else None
                if st == "ready":
                    if not b or b["state"] != "live":
                        v.append(("I-STATE", f"{tag} ready without a live binding ({b and b['state']})"))
                    if phase != "ready":
                        v.append(("I-STATE", f"{tag} ready with charge phase {phase}"))
                elif st == "parked":
                    if not b or b["state"] != "live":
                        v.append(("I-STATE", f"{tag} parked without a live binding ({b and b['state']})"))
                    if phase != "parked":
                        v.append(("I-STATE", f"{tag} parked with charge phase {phase}"))
                elif st == "stopped":
                    if b:
                        v.append(("I-STATE", f"{tag} stopped with a {b['state']} binding {b['id']}"))
                    if o:
                        v.append(("I-STATE", f"{tag} stopped with a {phase} charge"))
                if st == "failed" or st not in STABLE:
                    reason = (i.get("latest_operation") or {}).get("reason") or s.get("conditions")
                    self.track(f"inst:{s['id']}/{i.get('index')}:{st}", v, None, s["id"], f"{tag} {st}", reason)
        for b in led["runtime_bindings"]:
            if b["id"] not in seen_bindings and b["deployment_id"] in dep_by_id:
                v.append(("I-STATE", f"binding {b['id']} ({b['state']}) on {host_name(b['host_id'])} matches no instance"))
        for owner in owners:
            if owner not in seen_owners and owner.split("/")[0].replace("deployment:", "") in dep_by_id:
                v.append(("I-STATE", f"charge {owner} matches no instance"))
        # host probes: I-ORPHAN, I-GPU, identity liveness, I-DEAD, I-SAME
        probes = {}
        for host in HOSTS:
            hid = host_id(host)
            ids = []
            for b in led["runtime_bindings"]:
                if b["host_id"] == hid:
                    for i in b["identities"]:
                        ids.append({"pid": i["pid"], "ticks": i["start_ticks"], "boot": i["boot_id"], "role": i.get("role"),
                                    "binding": b["id"], "bstate": b["state"], "deployment": b["deployment_id"], "kind": "bound"})
            for i in self.expect_dead + self.expect_same:
                if i["host"] == host:
                    ids.append({**i, "kind": "dead" if i in self.expect_dead else "same", "owning": False})
            probes[host] = probe_host(host, ids)
        info["probes"] = probes
        for host, p in probes.items():
            if "error" in p:
                v.append(("I-PROBE", f"{host}: {p['error']}"))
                continue
            for o in p["orphans"]:
                v.append(("I-ORPHAN", f"{host}: engine process {o['pid']} ({o['cmd'][:100]}) belongs to no binding"))
            for g in p["gpu"]:
                if "error" in g:
                    v.append(("I-GPU", f"{host}: nvidia-smi {g['error']}"))
                elif not g["owned"]:
                    v.append(("I-GPU", f"{host}: GPU compute pid {g['pid']} ({g['mem']}, {g.get('cmd')}) is not owned"))
            if p["bytecode"]:
                v.append(("I-BYTECODE", f"{host}: bytecode in the runtime tree {p['bytecode'][:3]}"))
            for i in p["identities"]:
                if i["kind"] == "bound":
                    n = dep_by_id.get(i["deployment"], ("?",))[0]
                    s = sts.get(n) or {}
                    inst = [x for x in s.get("instances", []) if x.get("host_id") == host_id(host)]
                    st = inst[0].get("observed_state") if inst else None
                    if st in ("ready", "parked") and not i["alive"]:
                        v.append(("I-STATE", f"{n}@{host} {st} but owned {i['role']} pid {i['pid']} is gone"))
                    if i["alive"] and i.get("pstate") == "T" and st == "ready":
                        v.append(("I-STATE", f"{n}@{host} ready but {i['role']} pid {i['pid']} is stopped (T)"))
                elif i["kind"] == "dead" and i["alive"]:
                    # Still bound again (a restart reuses nothing, so a live identity here is the old process).
                    v.append(("I-DEAD", f"{host}: {i['role']} pid {i['pid']} of {i['deployment']} outlived the operation that ended it"))
                elif i["kind"] == "same":
                    b = [x for x in led["runtime_bindings"] if x["id"] == i["binding"]]
                    if b and not i["alive"]:
                        v.append(("I-SAME", f"{host}: {i['role']} pid {i['pid']} of {i['deployment']} was replaced while its binding {i['binding']} stayed"))
        # I-I1: every deployment with a Ready instance answers its golden
        i1 = {}
        for n, s in sts.items():
            if not s or not self.ready(s):
                continue
            d = self.deps[n]
            for attempt in range(max(1, len(self.ready(s)))):
                self.inflight += 1
                try:
                    proc = subprocess.run([sys.executable, os.path.join(HERE, "identity_probe.py"), "check",
                                           "--route", d["route"], "--model", d["model"], "--engine", d["engine"],
                                           "--goldens", os.path.join(RUNSTATE, "goldens.json"),
                                           "--prefix", str(I1_PREFIX),
                                           "--out", os.path.join(self.out, "i1.jsonl")],
                                          capture_output=True, text=True, timeout=900)
                finally:
                    self.inflight -= 1
                i1.setdefault(n, []).append(proc.stdout.strip()[-300:])
                if proc.returncode != 0:
                    v.append(("I-I1", f"{n}: {proc.stdout.strip()[-300:]} {proc.stderr.strip()[-200:]}"))
        info["i1"] = i1
        # I1, checkpoint half: every deployment of a model recorded the same measured digest
        digests = {}
        for n, s in sts.items():
            cd = (s or {}).get("checkpoint_digest") or {}
            if cd.get("state") == "mismatch":
                v.append(("I-I1", f"{n}: checkpoint digest mismatch {cd}"))
            if cd.get("state") == "recorded" and cd.get("digest"):
                digests.setdefault(self.deps[n]["model"], {})[n] = cd["digest"]
        for model, seen in digests.items():
            if len(set(seen.values())) > 1:
                v.append(("I-I1", f"model {model}: deployments recorded different checkpoint digests {seen}"))
        info["checkpoint_digests"] = digests
        # leases after the I1 probes: they must all have closed
        after = db_rows("SELECT id,deployment_id,disposition FROM request_leases WHERE disposition='inflight'")
        if after:
            v.append(("I-LEASE", f"in-flight leases after the I1 probes: {after}"))
        # expectations are checked once
        self.expect_dead, self.expect_same = [], []
        # forget uncertain items that cleared
        return v, info

    def track(self, key, v, age, dep_id, what, reason=None):
        first = self.first_seen.setdefault(key, now_ms())
        age = age if age is not None else (now_ms() - first) / 1000
        self._tracked.add(key)
        if age > UNCERTAIN_DEADLINE_S:
            journaled = journal_reason(dep_id, first) if dep_id else []
            if not reason and not journaled:
                v.append(("I-UNCERTAIN", f"{what} older than {UNCERTAIN_DEADLINE_S:.0f}s ({age:.0f}s) without a journaled reason"))
            else:
                v.append(("I-UNCERTAIN-NOTE", f"{what} older than {UNCERTAIN_DEADLINE_S:.0f}s ({age:.0f}s); reason {reason or journaled[:1]}"))

    def run(self, steps, start, max_hours):
        t0 = time.time()
        steps_path = os.path.join(self.out, "steps.jsonl")
        viol_path = os.path.join(self.out, "violations.jsonl")
        done = 0
        stop_reason = "steps"
        for step in range(start, start + steps):
            if time.time() - t0 > max_hours * 3600:
                stop_reason = "time budget"
                break
            rng = random.Random(self.seed * 1000003 + step)
            try:
                sts = self.statuses()
                led = ledger()
                ops, weights = zip(*self.OPS)
                order = rng.choices(ops, weights=weights, k=6)
                started = now_ms()
                outcome, detail, op = "skipped", {}, None
                for op in order:
                    outcome, detail = getattr(self, f"op_{op}")(rng, sts, led)
                    if outcome != "skipped":
                        break
                finished = now_ms()
                sts, moving = self.settle()
                self._tracked = set()
                viols, info = self.check(step, sts, moving)
                for key in list(self.first_seen):
                    if key not in self._tracked:
                        del self.first_seen[key]
            except SshLost as lost:
                stop_reason = f"ssh lost: {lost}"
                break
            record = {"step": step, "seed": self.seed, "op": op, "outcome": outcome, "started_ms": started,
                      "finished_ms": finished, "checked_ms": now_ms(),
                      "states": {n: self.inst_states(s) for n, s in sts.items()},
                      "ledger": info.get("ledger"), "violations": [f"{k}: {m}" for k, m in viols],
                      "detail": detail}
            with open(steps_path, "a") as h:
                h.write(json.dumps(record, sort_keys=True, default=str) + "\n")
            with open(os.path.join(self.out, "steps", f"{step:04d}.json"), "w") as h:
                json.dump({**record, "info": info}, h, indent=1, sort_keys=True, default=str)
            for k, m in viols:
                if k.endswith("-NOTE"):
                    continue
                self.violation_total += 1
                with open(viol_path, "a") as h:
                    h.write(json.dumps({"step": step, "op": op, "kind": k, "message": m}) + "\n")
            self.counts.setdefault(op, {}).setdefault(outcome, 0)
            self.counts[op][outcome] += 1
            done += 1
            print(f"{time.strftime('%H:%M:%S')} step {step} {op} {outcome} "
                  f"{json.dumps({n: [x[1] for x in self.inst_states(s)] for n, s in sts.items()})} "
                  f"violations={len([1 for k, _ in viols if not k.endswith('-NOTE')])}", flush=True)
            if os.path.exists(os.path.join(self.out, "STOP")):
                stop_reason = "STOP file"
                break
        summary = {"seed": self.seed, "from_step": start, "steps_done": done, "stop_reason": stop_reason,
                   "elapsed_s": round(time.time() - t0), "counts": self.counts, "violations": self.violation_total}
        with open(os.path.join(self.out, f"summary-{start}.json"), "w") as h:
            json.dump(summary, h, indent=1, sort_keys=True)
        print(json.dumps(summary), flush=True)
        return 3 if stop_reason.startswith("ssh lost") else (1 if self.violation_total else 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--plan", required=True)
    parser.add_argument("--steps", type=int, default=200)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--from-step", type=int, default=1)
    parser.add_argument("--max-hours", type=float, default=8)
    parser.add_argument("--check-only", action="store_true", help="settle and check the invariants once")
    args = parser.parse_args()
    with open(args.plan) as h:
        plan = json.load(h)
    out = os.path.join(EVID, "soak")
    os.makedirs(out, exist_ok=True)
    soak = Soak(plan, args.seed, out)
    if args.check_only:
        soak._tracked = set()
        sts, moving = soak.settle()
        viols, info = soak.check(0, sts, moving)
        print(json.dumps({"violations": viols, "info": info}, indent=1, default=str))
        return 1 if [k for k, _ in viols if not k.endswith("-NOTE")] else 0
    return soak.run(args.steps, args.from_step, args.max_hours)


if __name__ == "__main__":
    sys.exit(main())
