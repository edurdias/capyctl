#!/usr/bin/env python3
"""Requests, readings and verdicts for the multi-GPU live rows (ADR 0035 MG1-MG14).

Standard library only; runs on the rented host (remote.sh) and on the control
machine (run.sh verdict). The role's inference key and admin token are read
from its credentials file and never printed or written; engine keys are never
read. The role's ledger is opened read-only (mode=ro).

    mg.py chat       --cred F --endpoint URL --model M [--max-tokens N] [--out F] [--timeout S] [--no-key]
                     one request; prints `HTTP seconds ok|wrong|-` (ok: the default prompt's 42)
    mg.py hold       --cred F --endpoint URL --model M --max-tokens N --out F
                     one long streamed request that waits for its end (MG8's request in flight)
    mg.py bench      --cred F --endpoint URL --model M --concurrency C --tokens N --rounds R --out F [--no-key]
                     output tokens per second over every request of the rounds (JSON)
    mg.py get        FILE PATH            a value of a JSON file (a.b[0].c), any depth
    mg.py sql        DB QUERY             rows of a read-only SELECT, tab-separated
    mg.py find       FILE KEY             every value of KEY anywhere in a JSON file (JSON lines)
    mg.py owned      DB NAME [--role R | --rank N]
                     the ownership evidence of a deployment's live launches (JSON lines)
    mg.py gpus       [--pids P,P]         per-GPU totals and, for those PIDs, per-GPU bytes (JSON)
    mg.py engine-args --engine vllm|sglang LOG
                     the engine's own record of its arguments (vLLM `non-default args`, SGLang
                     `server_args`), less the per-launch keys (port, key, names), as JSON
    mg.py listeners  --pids P,P           TCP listeners of those PIDs, from /proc; with `ss -ltnp` beside
    mg.py events     DB [--kinds K,K] [--after N]   management events (JSON lines)
    mg.py charge     DB NAME              the ledger's reservations of a deployment, per domain (JSON)
    mg.py shm        --out F              the names in /dev/shm (sorted)
    mg.py hf-rev     REPO                 the commit `main` names on Hugging Face
    mg.py fit        LOG...               llama.cpp's per-device buffer lines from an engine log
    mg.py verdict    RESULTS_TSV OUT_MD [--settings F]
"""
import argparse
import http.client
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

# Qwen3's soft switch: without it the model reasons first and 16 tokens end
# inside the reasoning, before the answer.
DEFAULT_PROMPT = "What is 17+25? Answer with only the number. /no_think"
LONG_PROMPT = "Write a long story about a lighthouse keeper, at least two thousand words."


def credential(path, name):
    """One `name: value` line of the role's credentials file; never printed."""
    for line in Path(path).read_text().splitlines():
        m = re.match(rf"^\s*{name}\s*:\s*(\S+)\s*$", line)
        if m:
            return m.group(1)
    raise SystemExit(f"no {name} line in the credentials file")


def host_port(url):
    host, port = re.match(r"^http://([^/:]+):(\d+)", url).groups()
    return host, int(port)


def headers_for(a):
    h = {"Content-Type": "application/json"}
    if not a.no_key:
        h["Authorization"] = "Bearer " + credential(a.cred, "api_key")
    return h


def body_for(a, prompt, max_tokens, stream=False):
    body = {"model": a.model, "temperature": 0, "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": prompt}]}
    if stream:
        body["stream"] = True
        body["stream_options"] = {"include_usage": True}
    return body


def request(a, body, timeout):
    host, port = host_port(a.endpoint)
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    t0 = time.perf_counter()
    conn.request("POST", "/v1/chat/completions", body=json.dumps(body), headers=headers_for(a))
    resp = conn.getresponse()
    return conn, resp, t0


def dump(path, obj):
    if path:
        Path(path).write_text(json.dumps(obj, indent=2, sort_keys=True) + "\n")


def cmd_chat(a):
    try:
        conn, resp, t0 = request(a, body_for(a, DEFAULT_PROMPT, a.max_tokens), a.timeout)
        raw = resp.read()
        dt = time.perf_counter() - t0
        conn.close()
    except (OSError, http.client.HTTPException) as e:
        print(f"000 0.0 - ({type(e).__name__})")
        return 1
    verdict = "-"
    try:
        doc = json.loads(raw)
        text = doc["choices"][0]["message"].get("content") or ""
        verdict = "ok" if "42" in text else "wrong"
    except (ValueError, KeyError, IndexError, TypeError):
        doc = {"raw": raw.decode(errors="replace")[:2000]}
    dump(a.out, {"status": resp.status, "seconds": round(dt, 3), "body": doc,
                 "request_id": resp.getheader("x-request-id")})
    print(f"{resp.status} {dt:.2f} {verdict}")
    return 0 if resp.status == 200 else 1


def cmd_hold(a):
    """A streamed request kept open until it ends or fails; records when each part came."""
    record = {"started": time.time(), "chunks": 0, "first_chunk_s": None, "status": None, "error": None}
    try:
        conn, resp, t0 = request(a, body_for(a, LONG_PROMPT, a.max_tokens, stream=True), 3600)
        record["status"] = resp.status
        while True:
            line = resp.readline()
            if not line:
                break
            if line.startswith(b"data:"):
                record["chunks"] += 1
                if record["first_chunk_s"] is None:
                    record["first_chunk_s"] = round(time.perf_counter() - t0, 3)
        conn.close()
    except (OSError, http.client.HTTPException) as e:
        record["error"] = type(e).__name__
    record["ended"] = time.time()
    dump(a.out, record)
    return 0


def cmd_bench(a):
    """C concurrent clients, R rounds each, N output tokens per request (ignore_eos)."""
    results, lock = [], threading.Lock()

    def one(i):
        body = body_for(a, f"Request {i}: " + LONG_PROMPT, a.tokens)
        body["ignore_eos"] = True
        try:
            conn, resp, t0 = request(a, body, 1800)
            raw = resp.read()
            dt = time.perf_counter() - t0
            conn.close()
            tokens = json.loads(raw).get("usage", {}).get("completion_tokens", 0) if resp.status == 200 else 0
            with lock:
                results.append({"status": resp.status, "seconds": dt, "tokens": tokens})
        except (OSError, http.client.HTTPException, ValueError) as e:
            with lock:
                results.append({"status": 0, "seconds": 0, "tokens": 0, "error": type(e).__name__})

    # One warm-up request, not counted.
    one(-1)
    results.clear()
    t0 = time.perf_counter()
    for r in range(a.rounds):
        threads = [threading.Thread(target=one, args=(r * a.concurrency + c,)) for c in range(a.concurrency)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    wall = time.perf_counter() - t0
    tokens = sum(x["tokens"] for x in results)
    ok = sum(1 for x in results if x["status"] == 200)
    out = {"concurrency": a.concurrency, "rounds": a.rounds, "tokens_per_request": a.tokens,
           "requests": len(results), "ok": ok, "output_tokens": tokens, "wall_s": round(wall, 2),
           "tokens_per_s": round(tokens / wall, 2) if wall else 0}
    dump(a.out, out)
    print(json.dumps(out, sort_keys=True))
    return 0 if ok == len(results) else 1


def walk(doc, path):
    for part in re.findall(r"[^.\[\]]+|\[\d+\]", path):
        if part.startswith("["):
            idx = int(part[1:-1])
            doc = doc[idx] if isinstance(doc, list) and idx < len(doc) else None
        else:
            doc = doc.get(part) if isinstance(doc, dict) else None
        if doc is None:
            return None
    return doc


def cmd_find(a):
    def visit(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key == a.key:
                    print(json.dumps(value, sort_keys=True))
                visit(value)
        elif isinstance(node, list):
            for value in node:
                visit(value)
    try:
        visit(json.loads(Path(a.file).read_text()))
    except (OSError, ValueError):
        pass
    return 0


# Per launch, never part of the comparison: ports, keys, names, paths of the run.
VOLATILE = {"port", "host", "api_key", "served_model_name", "model", "model_path", "tokenizer_path",
            "download_dir", "random_seed", "seed", "dist_init_addr", "nccl_port", "file_storage_path",
            "base_gpu_id", "log_level", "enable_metrics", "uvicorn_log_level"}


def cmd_engine_args(a):
    import ast
    text = Path(a.log).read_text(errors="replace")
    parsed = {}
    if a.engine == "vllm":
        for m in re.finditer(r"non-default args: (\{.*\})", text):
            try:
                parsed = ast.literal_eval(m.group(1))
            except (ValueError, SyntaxError):
                continue
    else:
        for m in re.finditer(r"server_args=ServerArgs\((.*)\)\s*$", text, re.M):
            parsed = {}
            for k, v in re.findall(r"(\w+)=('(?:[^']*)'|\"(?:[^\"]*)\"|\[[^\]]*\]|\{[^}]*\}|[^,]+)", m.group(1)):
                try:
                    parsed[k] = ast.literal_eval(v)
                except (ValueError, SyntaxError):
                    parsed[k] = v.strip()
    print(json.dumps({k: v for k, v in parsed.items() if k not in VOLATILE}, sort_keys=True, default=str))
    return 0 if parsed else 1


def cmd_get(a):
    try:
        value = walk(json.loads(Path(a.file).read_text()), a.path)
    except (OSError, ValueError):
        value = None
    if value is None:
        print("")
    elif isinstance(value, (dict, list)):
        print(json.dumps(value, sort_keys=True))
    else:
        print(str(value).lower() if isinstance(value, bool) else value)
    return 0


def ro(db):
    return sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=10)


def rows(db, query, args=()):
    conn = ro(db)
    conn.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in conn.execute(query, args)]
    finally:
        conn.close()


def cmd_sql(a):
    if not re.match(r"^\s*select\b", a.query, re.I):
        raise SystemExit("read-only SELECTs only")
    for r in rows(a.db, a.query):
        print("\t".join("" if v is None else str(v) for v in r.values()))
    return 0


def deployment_id(db, name):
    found = rows(db, "SELECT id FROM deployments WHERE name=?", (name,))
    if not found:
        raise SystemExit(f"no deployment {name}")
    return found[0]["id"]


def cmd_owned(a):
    """The identities a deployment's live launches recorded (ADR 0027; ADR 0035 §6).

    A rank process is `rank-N`, or an `api`/`worker-N` process the readiness receipt
    names (`tp ranks: 0=worker-0:11@gpu0 1=worker-2:13@gpu1`, in the binding)."""
    ident = deployment_id(a.db, a.name)
    for b in rows(a.db, "SELECT id,state,binding_json,identities_json FROM runtime_bindings "
                        "WHERE deployment_id=? AND state IN ('live','uncertain')", (ident,)):
        # The receipt rides the binding or the journal's evidence; a PID only counts
        # when it is one of this binding's own identities (below).
        texts = [b["binding_json"] or ""] + [r["evidence"] or "" for r in rows(
            a.db, "SELECT evidence FROM journal_entries WHERE evidence LIKE '%tp ranks: %' ORDER BY recorded_at")]
        ranks = {}
        for text in texts:
            for entry in re.findall(r"tp ranks: ((?:\d+=[\w-]+:\d+@gpu\d+ ?)+)", text):
                for rank, _role, pid, gpu in re.findall(r"(\d+)=([\w-]+):(\d+)@(gpu\d+)", entry):
                    ranks[int(pid)] = (int(rank), gpu)
        for identity in json.loads(b["identities_json"] or "[]"):
            role = identity.get("role", "")
            rank, gpu = ranks.get(identity.get("pid"), (None, None))
            if rank is None and re.match(r"^rank-\d+$", role):
                rank = int(role.split("-")[1])
            if a.role and role != a.role:
                continue
            if a.rank is not None and rank != a.rank:
                continue
            print(json.dumps({"binding_id": b["id"], "binding_state": b["state"], "rank": rank, "gpu": gpu,
                              **identity}, sort_keys=True))
    return 0


def smi(query, kind):
    out = subprocess.run(["nvidia-smi", f"--query-{kind}={query}", "--format=csv,noheader,nounits"],
                         capture_output=True, text=True, timeout=30, check=True).stdout
    return [[c.strip() for c in line.split(",")] for line in out.splitlines() if line.strip()]


def cmd_gpus(a):
    gpus = {u: {"index": int(i), "uuid": u, "name": n, "used_mib": int(used), "total_mib": int(total)}
            for i, u, n, used, total in smi("index,uuid,name,memory.used,memory.total", "gpu")}
    pids = {int(p) for p in a.pids.split(",") if p} if a.pids else set()
    per = {}
    for pid, uuid, used in smi("pid,gpu_uuid,used_memory", "compute-apps"):
        if int(pid) in pids and uuid in gpus:
            key = f"gpu{gpus[uuid]['index']}"
            per.setdefault(key, {})[pid] = int(used)
    out = {"gpus": sorted(gpus.values(), key=lambda g: g["index"]),
           "owned_mib": {k: sum(v.values()) for k, v in sorted(per.items())},
           "owned_by_pid": per}
    print(json.dumps(out, sort_keys=True))
    return 0


def socket_inodes(pid):
    inodes = set()
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                target = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                continue
            m = re.match(r"^socket:\[(\d+)\]$", target)
            if m:
                inodes.add(m.group(1))
    except OSError:
        pass
    return inodes


def tcp_listeners():
    found = {}
    for path, family in (("/proc/net/tcp", socket.AF_INET), ("/proc/net/tcp6", socket.AF_INET6)):
        try:
            lines = Path(path).read_text().splitlines()[1:]
        except OSError:
            continue
        for line in lines:
            f = line.split()
            if f[3] != "0A":  # LISTEN
                continue
            addr_hex, port_hex = f[1].split(":")
            raw = bytes.fromhex(addr_hex)
            if family == socket.AF_INET:
                addr = socket.inet_ntop(family, raw[::-1])
            else:
                addr = socket.inet_ntop(family, b"".join(raw[i:i + 4][::-1] for i in range(0, 16, 4)))
            found[f[9]] = (addr, int(port_hex, 16))
    return found


def cmd_listeners(a):
    """Every TCP listener the PIDs hold, from /proc (the check's own method), and `ss -ltnp`."""
    listeners = tcp_listeners()
    held = []
    for pid in [int(p) for p in a.pids.split(",") if p]:
        for inode in socket_inodes(pid):
            if inode in listeners:
                addr, port = listeners[inode]
                held.append({"pid": pid, "address": addr, "port": port,
                             "loopback": addr in ("127.0.0.1", "::1")})
    try:
        ss = subprocess.run(["ss", "-H", "-ltnp"], capture_output=True, text=True, timeout=30).stdout
    except OSError:
        ss = ""
    pids = {str(p) for p in a.pids.split(",") if p}
    ss_rows = [line for line in ss.splitlines() if any(f"pid={p}," in line for p in pids)]
    ss_off = [line for line in ss_rows if not re.search(r"\s(127\.0\.0\.1|\[::1\]):\d+\s", line)]
    out = {"proc": held, "off_loopback": [h for h in held if not h["loopback"]],
           "ss_rows": ss_rows, "ss_off_loopback": ss_off}
    print(json.dumps(out, sort_keys=True))
    return 0


def cmd_events(a):
    kinds = set(a.kinds.split(",")) if a.kinds else None
    for e in rows(a.db, "SELECT sequence,recorded_at_ms,kind,deployment_id,payload_json FROM management_events "
                        "WHERE sequence>? ORDER BY sequence", (a.after,)):
        if kinds and e["kind"] not in kinds:
            continue
        e["payload"] = json.loads(e.pop("payload_json") or "{}")
        print(json.dumps(e, sort_keys=True))
    # ADR 0035 §7: engine_rank_exited and tp_stalled are journal states (the
    # instance's last error), not management events: the journal entries of
    # those kinds recorded after the event sequence `after` was.
    if kinds:
        since = rows(a.db, "SELECT recorded_at_ms FROM management_events WHERE sequence=?", (a.after,))
        since_ms = since[0]["recorded_at_ms"] if since else 0
        stamp = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(since_ms / 1000)) + f".{since_ms % 1000:03d}Z"
        marks = ",".join("?" * len(kinds))
        for j in rows(a.db, f"SELECT recorded_at,state,evidence FROM journal_entries WHERE state IN ({marks}) "
                            "AND recorded_at>=? ORDER BY recorded_at", (*sorted(kinds), stamp)):
            print(json.dumps({"source": "journal", "kind": j["state"], "recorded_at": j["recorded_at"],
                              "evidence": j["evidence"]}, sort_keys=True))
    return 0


def cmd_charge(a):
    # The resource ledger: one footprint per owner (instance 0 is the deployment
    # id, another instance `deployment:<id>/instance:<k>`), its allocations
    # (domain, bytes, host KV bytes) in the owner's current phase; no row, or no
    # allocation, is no charge. The `reservations` table is the F1 skeleton and
    # stays empty.
    ident = deployment_id(a.db, a.name)
    out = {}
    for r in rows(a.db, "SELECT owner_id,footprint_json FROM resource_owners "
                        "WHERE owner_id=? OR owner_id LIKE ?", (ident, f"deployment:{ident}/instance:%")):
        footprint = json.loads(r["footprint_json"])
        for domain, nbytes, host_kv in footprint.get("allocations", []):
            out.setdefault(domain, []).append({"owner": r["owner_id"], "bytes": nbytes, "host_kv_bytes": host_kv,
                                               "phase": footprint.get("phase")})
    print(json.dumps(out, sort_keys=True))
    return 0


def cmd_shm(a):
    names = sorted(os.listdir("/dev/shm")) if os.path.isdir("/dev/shm") else []
    Path(a.out).write_text("\n".join(names) + ("\n" if names else ""))
    return 0


def cmd_hf_rev(a):
    url = f"https://huggingface.co/api/models/{a.repo}/revision/main"
    with urllib.request.urlopen(url, timeout=60) as resp:
        print(json.load(resp)["sha"])
    return 0


def cmd_fit(a):
    """llama.cpp's per-device lines: the model and KV buffers each CUDA device holds."""
    pattern = re.compile(r"(CUDA\d+) (model|KV|compute) buffer size\s*=\s*([\d.]+) MiB|llama_params_fit|split_mode|tensor_split")
    for log in a.logs:
        for line in Path(log).read_text(errors="replace").splitlines():
            if pattern.search(line):
                print(line.strip())
    return 0


def cmd_verdict(a):
    results = [line.split("\t") for line in Path(a.results).read_text().splitlines() if line.strip()] \
        if Path(a.results).exists() else []
    settings = json.loads(Path(a.settings).read_text()) if a.settings and Path(a.settings).exists() else {}
    rows_by_id = {}
    for r in results:
        rows_by_id.setdefault(r[0], []).append(r)
    order = [f"MG{i}" for i in range(1, 15)] + ["BW"]
    lines = ["# Multi-GPU live rows (ADR 0035)", ""]
    for key in ("commit", "machine", "engines", "started", "ended"):
        if key in settings:
            lines.append(f"- {key}: {settings[key]}")
    lines += ["", "| Row | Part | Result | Evidence |", "|---|---|---|---|"]
    overall = {}
    for rid in order + sorted(set(rows_by_id) - set(order)):
        for r in rows_by_id.get(rid, []):
            part, res, detail = (r + ["", "", ""])[1:4]
            lines.append(f"| {rid} | {part} | {res.upper()} | {detail.replace('|', '/')} |")
            prev = overall.get(rid)
            overall[rid] = "fail" if "fail" in (prev, res) else ("skip" if res == "skip" and prev in (None, "skip") else "pass")
    lines += ["", "## Per row", ""]
    for rid in order:
        lines.append(f"- {rid}: {overall.get(rid, 'not run').upper()}")
    lines += ["", "CPU and Fake-engine tests are not qualification; these rows are the live check.", ""]
    Path(a.out).write_text("\n".join(lines))
    print("\n".join(lines))
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    def client(name):
        s = sub.add_parser(name)
        s.add_argument("--cred")
        s.add_argument("--endpoint", required=True)
        s.add_argument("--model", required=True)
        s.add_argument("--out")
        s.add_argument("--no-key", action="store_true")
        return s

    s = client("chat")
    s.add_argument("--max-tokens", type=int, default=32)
    s.add_argument("--timeout", type=float, default=1800)
    s = client("hold")
    s.add_argument("--max-tokens", type=int, default=4096)
    s = client("bench")
    s.add_argument("--concurrency", type=int, required=True)
    s.add_argument("--tokens", type=int, default=256)
    s.add_argument("--rounds", type=int, default=3)
    s = sub.add_parser("get")
    s.add_argument("file")
    s.add_argument("path")
    s = sub.add_parser("sql")
    s.add_argument("db")
    s.add_argument("query")
    s = sub.add_parser("owned")
    s.add_argument("db")
    s.add_argument("name")
    s.add_argument("--role")
    s.add_argument("--rank", type=int)
    s = sub.add_parser("gpus")
    s.add_argument("--pids", default="")
    s = sub.add_parser("listeners")
    s.add_argument("--pids", required=True)
    s = sub.add_parser("events")
    s.add_argument("db")
    s.add_argument("--kinds")
    s.add_argument("--after", type=int, default=0)
    s = sub.add_parser("charge")
    s.add_argument("db")
    s.add_argument("name")
    s = sub.add_parser("shm")
    s.add_argument("--out", required=True)
    s = sub.add_parser("hf-rev")
    s.add_argument("repo")
    s = sub.add_parser("fit")
    s.add_argument("logs", nargs="+")
    s = sub.add_parser("verdict")
    s.add_argument("results")
    s.add_argument("out")
    s.add_argument("--settings")
    s = sub.add_parser("find")
    s.add_argument("file")
    s.add_argument("key")
    s = sub.add_parser("engine-args")
    s.add_argument("--engine", required=True, choices=["vllm", "sglang"])
    s.add_argument("log")
    a = p.parse_args()
    return globals()["cmd_" + a.cmd.replace("-", "_")](a)


if __name__ == "__main__":
    sys.exit(main())
