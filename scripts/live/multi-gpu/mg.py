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
    mg.py engine-args --engine vllm|sglang LOG [--argv FILE]
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
import tempfile
import time
import urllib.request
from pathlib import Path

# Qwen3's soft switch: without it the model reasons first and 16 tokens end
# inside the reasoning, before the answer.
DEFAULT_PROMPT = "What is 17+25? Answer with only the number. /no_think"
LONG_PROMPT = "Write a long story about a lighthouse keeper, at least two thousand words."


def credential(path, name):
    """Standalone YAML-like lines or enrolled-server JSON; never printed."""
    text = Path(path).read_text()
    if text.lstrip().startswith("{"):
        value = json.loads(text).get(name)
        if isinstance(value, str) and value:
            return value
        raise SystemExit(f"no {name} in the credentials file")
    for line in text.splitlines():
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
    text = Path(a.log).read_text(errors="replace") if Path(a.log).is_file() else ""
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
        for match in re.finditer(r"^MG_EFFECTIVE_ARGS=(.*)$", text, re.M):
            parsed = json.loads(match.group(1))
    tokens = []
    if a.pid:
        try:
            tokens = Path(f"/proc/{a.pid}/cmdline").read_bytes().decode().split("\0")
        except OSError:
            pass
    elif a.argv and Path(a.argv).is_file():
        tokens = Path(a.argv).read_text().splitlines()
    # Compare vLLM argv with argv (not its differently typed non-default log
    # summary). Read it in memory only: no credential-bearing argv artifact.
    if tokens and (not parsed or a.engine == "vllm"):
        if "serve" in tokens:
            parsed = {}
            rest = tokens[tokens.index("serve") + 2:]
            i = 0
            while i < len(rest):
                tok = rest[i]
                if tok.startswith("--"):
                    key = tok[2:].split("=", 1)[0].replace("-", "_")
                    if "=" in tok:
                        value, i = tok.split("=", 1)[1], i + 1
                    elif i + 1 < len(rest) and rest[i + 1] and not rest[i + 1].startswith("--"):
                        value, i = rest[i + 1], i + 2
                    else:
                        value, i = True, i + 1
                    if isinstance(value, str):
                        try:
                            decoded = json.loads(value)
                            # An explicit scalar `false`/`null` is an argv value,
                            # not an absent flag. Bare flags alone are True.
                            if decoded is not None and not isinstance(decoded, bool):
                                value = decoded
                        except ValueError:
                            pass
                    # `--capyctl-user-args` is CapyCTL's own marker between its
                    # reserved arguments and the user's (runtime/vllm_entry.py).
                    if not re.search(r"(^|_)(key|secret|token)$", key) and not key.startswith("capyctl"):
                        parsed[key] = value
                else:
                    i += 1
        elif "--public-settings-json" in tokens:
            # SGLang under CapyCTL logs no `server_args`: CapyCTL's entry composes
            # them from the public launch settings (argv; the keys ride separate
            # descriptors). The same composition, from the role's own runtime copy
            # (runtime/sglang_server_args.py), less the keys, paths and endpoint.
            entry = next(t for t in tokens if t.endswith("/sglang_entry.py"))
            sys.path.insert(0, str(Path(entry).parent.parent))
            from runtime import sglang_server_args as s
            public = json.loads(tokens[tokens.index("--public-settings-json") + 1])
            settings = public["settings"]
            available = s.available_bytes_for(settings["memory"])
            parsed = dict(s._SAFE_DEFAULTS)
            parsed.update(s.typed_keywords(settings))
            parsed.update(s._RESERVED_CONSTANT)
            parsed.update(mem_fraction_static=s.static_fraction(s.fraction_bytes(settings["memory"], available), available),
                          enable_memory_saver=settings["memory_saver"],
                          enable_weights_cpu_backup=settings["cpu_weight_backup"])
            if public.get("devices") is not None:
                parsed.update(tp_size=len(public["devices"]), base_gpu_id=0)
    print(json.dumps({k: v for k, v in parsed.items() if k not in VOLATILE and
                      not re.search(r"(^|_)(key|secret|token)$", k)}, sort_keys=True, default=str))
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
        # The receipt rides the binding, its steps' evidence (SGLang's `tp ranks:
        # 0=worker-1:...` names its scheduler processes) or the journal's; a PID
        # only counts when it is one of this binding's own identities (below).
        texts = [b["binding_json"] or ""] + [r["evidence_json"] or "" for r in rows(
            a.db, "SELECT e.evidence_json FROM lifecycle_evidence e JOIN lifecycle_steps s ON s.id=e.step_id "
                  "WHERE s.binding_id=?", (b["id"],))] + [r["evidence"] or "" for r in rows(
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


def cmd_rank_exit(a):
    """An observed exit set must name the targeted rank, in any order."""
    for line in Path(a.events).read_text().splitlines():
        event = json.loads(line)
        if event.get("kind") == "engine_rank_exited" and re.search(
                rf"\brank\s+{a.rank}\b", json.dumps(event)):
            return 0
    return 1


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
    pattern = re.compile(r"CUDA\d+\s+(?:model|KV|compute) buffer size\s*=\s*[\d.]+\s+MiB|llama_params_fit|split_mode|tensor_split")
    for log in a.logs:
        for line in Path(log).read_text(errors="replace").splitlines():
            if pattern.search(line):
                print(line.strip())
    return 0


EXPECTED = {
    "MG1": ("ready", "charge", "listeners", "peaks"),
    "MG2": ("ready", "charge", "listeners", "peaks"),
    "MG3": ("ready", "charge", "listeners", "peaks", "fit"),
    "MG4": ("deep",), "MG5": ("deep",),
    "MG6": ("vllm", "sglang", "released"),
    "MG7": ("vllm stop", "vllm kill", "sglang stop", "sglang kill"),
    "MG8": ("vllm", "sglang"), "MG9": ("vllm", "sglang"),
    "MG10": ("switch",), "MG11": ("remote", "old"),
    "MG12": ("tp3", "pin"), "MG13": ("n1", "n2", "wake"),
    "MG14": ("tp1", "auto"), "BW": ("q30",),
}


def fit_matches(text, charge):
    """Require llama.cpp's own positive model allocation on each granted GPU."""
    try:
        buffers = {}
        for gpu, mib in re.findall(r"CUDA(\d+)\s+model buffer size\s*=\s*([\d.]+)\s+MiB", text):
            buffers[f"gpu{gpu}"] = max(buffers.get(f"gpu{gpu}", 0), float(mib) * 2**20)
        grants = {}
        for domain, records in charge.items():
            match = re.search(r"gpu(\d+)", domain)
            if match:
                grants[f"gpu{match.group(1)}"] = max((r["bytes"] for r in records), default=0)
        return all(0 < buffers.get(gpu, 0) <= grants.get(gpu, 0) for gpu in ("gpu0", "gpu1"))
    except (AttributeError, KeyError, TypeError, ValueError):
        return False


def cmd_fit_check(a):
    ok = fit_matches(Path(a.log).read_text(), json.loads(Path(a.charge).read_text()))
    print("CUDA0/CUDA1 model buffers fit their grants" if ok else
          "missing, zero, or over-grant CUDA0/CUDA1 model-buffer evidence")
    return 0 if ok else 1


def row_evidence(rid, settings):
    if rid in ("MG1", "MG2", "MG3"):
        files = [f"{rid}-{suffix}" for suffix in
                 ("status.json", "gpus.json", "charge.json", "listeners.json", "peaks.json")]
        files.append(f"chat-{rid}.json")
        if rid == "MG3":
            files.append("MG3-fit.txt")
        return files
    if rid in ("MG4", "MG5", "MG6"):
        labels = ("vllm", "sglang") if rid == "MG6" else ("deep",)
        count = 3 if rid == "MG6" else 5
        files = [f"{rid}-{label}-{phase}{i}-{kind}.json"
                 for label in labels for i in range(1, count + 1)
                 for phase, kind in (("park", "gpus"), ("park", "charge"), ("park", "status"),
                                     ("wake", "gpus"), ("wake", "status"))]
        files += [f"chat-{rid}-{label}-wake{i}.json" for label in labels for i in range(1, count + 1)]
        if rid == "MG6":
            files += [f"mg6-{label}-charge-after.json" for label in labels]
        return files
    if rid == "MG7":
        return [f"mg7-{engine}-{suffix}" for engine in ("vllm", "sglang") for suffix in
                ("events.jsonl", "shm-before.txt", "shm-after.txt", "shm-after-kill.txt")]
    if rid == "MG8":
        return [f"mg8-{engine}-{suffix}" for engine in ("vllm", "sglang") for suffix in
                ("events.jsonl", "hold.json")]
    if rid == "MG9":
        return [f"mg9-{engine}-{side}-c{c}.json" for engine in ("vllm", "sglang")
                for side in ("capyctl", "hand") for c in (1, 4, 8)] + [
                    f"mg9-{engine}-{suffix}" for engine in ("vllm", "sglang")
                    for suffix in ("args.json", "hand-args.json", "hand.log")]
    return {
        "MG10": ["mg10-events.jsonl", "chat-mg10.json"],
        "MG11": ["mg11-new-start.out", "mg11-old-deploy.out", "mg11-old-start.out", "chat-mg11.json"],
        "MG12": ["MG12-tp3.out", "MG12-pin.out"],
        "MG13": ["mg13-4b.json", "mg13-30b.json", "mg13-30b-woken.json",
                 "mg13-pinned-args.json", "mg13-n1-args.json", "mg13-n2-args.json", "mg13-baseline-args.json",
                 "chat-mg13-wake.json"],
        "MG14": ["MG14-tp1.out", "MG14-auto.out"],
        "BW": ["sources-bw-q30.json"],
    }[rid]


def evidence_role(rid):
    return "srv" if rid == "MG11" else "s2" if rid in ("MG4", "MG5", "MG6", "MG10", "MG13") else "s1"


def required_evidence(rid, settings):
    role = evidence_role(rid)
    return row_evidence(rid, settings) + ["versions.txt", f"events-{role}.jsonl", f"journal-{role}.jsonl"]


def selected_rows(only):
    return set(EXPECTED) if only.lower() == "all" else set(only.upper().split(","))


def read_results(path):
    results, errors = {}, []
    if path.exists():
        for number, line in enumerate(path.read_text().splitlines(), 1):
            fields = line.split("\t", 3)
            if len(fields) != 4 or fields[0] not in EXPECTED or fields[2] not in ("pass", "fail", "skip"):
                errors.append(f"{path.name}:{number}: malformed result")
                continue
            results.setdefault(fields[0], []).append(fields)
    return results, errors


def compatible_attempt(path, settings):
    try:
        other = json.loads((path / "settings.json").read_text())
        keys = ("commit", "harness", "cycles", "hb_cycles", "stall")
        return all(other.get(k) == settings.get(k) for k in keys)
    except (OSError, ValueError):
        return False


def historical_errors(path, raw):
    errors = []
    try:
        if (path / "raw/versions.txt").read_text() != (raw / "versions.txt").read_text():
            errors.append("engine fingerprints differ")
    except OSError:
        errors.append("engine fingerprints missing")
    try:
        host = json.loads((path / "host-completion.json").read_text())
        completion = json.loads((path / "completion.json").read_text())
        if (host.get("exit_code") != 0 or completion.get("state") != "complete"
                or completion.get("host_exit") != 0 or completion.get("final_collection") is not True):
            errors.append("historical attempt did not complete successfully")
    except (OSError, ValueError):
        errors.append("historical completion evidence missing")
    return errors


def cmd_verdict(a):
    raw = Path(a.results).parent
    sources, errors = {}, []
    if getattr(a, "host_status", None) not in (None, "0"):
        errors.append("host completion missing or unsuccessful")
    if getattr(a, "collection_status", None) not in (None, "1"):
        errors.append("final evidence collection incomplete")
    try:
        settings = json.loads(Path(a.settings).read_text()) if a.settings else {}
    except (OSError, ValueError):
        settings = {}
        errors.append("missing or malformed settings.json")
    # Latest *selected* attempt owns a row, including a row it failed to produce.
    # Missing/failed reruns never resurrect an earlier pass; other commits never mix.
    if a.attempts:
        for attempt in sorted(Path(a.attempts).iterdir()):
            if not attempt.is_dir() or attempt.resolve() == raw.parent.resolve():
                continue
            if not compatible_attempt(attempt, settings):
                continue
            prior = json.loads((attempt / "settings.json").read_text())
            records, malformed = read_results(attempt / "raw/results.tsv")
            malformed.extend(historical_errors(attempt, raw))
            for rid in selected_rows(prior.get("only", "all")):
                result = records.get(rid, [])
                if malformed:
                    result = result + [[rid, "format", "fail", "; ".join(malformed)]]
                sources[rid] = (result, attempt / "raw", prior)
    records, malformed = read_results(Path(a.results))
    errors.extend(malformed)
    for rid in selected_rows(settings.get("only", a.only)):
        sources[rid] = (records.get(rid, []), raw, settings)
    rows, lines = {}, ["# Multi-GPU live rows (ADR 0035)", ""]
    for key in ("commit", "started", "ended", "only"):
        if key in settings:
            lines.append(f"- {key}: {settings[key]}")
    lines += ["", "| Row | Part | Result | Evidence |", "|---|---|---|---|"]
    for rid, parts in EXPECTED.items():
        result, evidence, row_settings = sources.get(rid, ([], raw, settings))
        present = {r[1] for r in result}
        missing = sorted(set(parts) - present)
        required = required_evidence(rid, row_settings)
        missing_files = [f for f in required if not (evidence / f).is_file()]
        empty_files = [f for f in required
                       if (evidence / f).is_file() and not (evidence / f).stat().st_size and "-shm-" not in f
                       and not f.startswith(("events-", "journal-"))]
        invalid_files = []
        if rid not in ("MG12", "MG14", "BW"):
            role = "h1" if rid == "MG11" else evidence_role(rid)
            if not any(p.is_file() and p.stat().st_size for p in (evidence / "engine-logs" / role).rglob("*")):
                missing_files.append(f"engine-logs/{role}/")
        for name in required:
            if name.endswith((".json", ".jsonl")) and name not in missing_files:
                try:
                    text = (evidence / name).read_text()
                    if name.endswith(".jsonl"):
                        for line in text.splitlines():
                            if line.strip():
                                json.loads(line)
                    else:
                        json.loads(text)
                except ValueError:
                    invalid_files.append(name)
        passed = (bool(result) and not missing and not missing_files and not empty_files and
                  not invalid_files and all(r[2] == "pass" for r in result) and bool(row_settings.get("commit")))
        if rid == "MG3" and passed:
            passed = fit_matches((evidence / "MG3-fit.txt").read_text(),
                                 json.loads((evidence / "MG3-charge.json").read_text()))
        if rid in ("MG4", "MG5") and int(row_settings.get("cycles", 0)) < 5:
            passed = False
        if rid == "MG6" and int(row_settings.get("hb_cycles", 0)) < 3:
            passed = False
        rows[rid] = {"result": "pass" if passed else "fail", "missing_parts": missing,
                     "missing_evidence": missing_files, "empty_evidence": empty_files, "invalid_evidence": invalid_files,
                     "attempt": evidence.parent.name}
        for _, part, res, detail in result:
            lines.append(f"| {rid} | {part} | {res.upper()} | {detail.replace('|', '/')} |")
        if missing or missing_files or empty_files or invalid_files:
            lines.append(f"| {rid} | completeness | FAIL | Missing parts: {', '.join(missing)}; missing/empty/invalid evidence: {', '.join(missing_files + empty_files + invalid_files)} |")
    selected = selected_rows(a.only)
    selected_pass = not errors and all(rows[rid]["result"] == "pass" for rid in selected)
    qualified = not errors and all(row["result"] == "pass" for row in rows.values())
    report = {"qualified": qualified, "selected_pass": selected_pass, "rows": rows, "errors": errors}
    lines += ["", "## Per row", ""] + [f"- {rid}: {r['result'].upper()} (attempt {r['attempt']})" for rid, r in rows.items()]
    if errors:
        lines += ["", "## Run errors", ""] + [f"- {error}" for error in errors]
    lines += ["", f"All-row qualification: {'PASS' if qualified else 'FAIL'}", "",
              "CPU and Fake-engine tests are not qualification; these rows are the live check.", ""]
    atomic_text(Path(a.out), "\n".join(lines))
    atomic_text(Path(a.out).with_suffix(".json"), json.dumps(report, indent=2) + "\n")
    print("\n".join(lines))
    return 0 if selected_pass else 1


def atomic_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.is_file() and path.read_text(errors="replace") == text:
        return
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".evidence-", delete=False) as stream:
        tmp = Path(stream.name)
        stream.write(text)
        stream.flush()
        os.fsync(stream.fileno())
    tmp.replace(path)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def sanitize(text, secrets=(), host=""):
    for secret in sorted(set(secrets), key=len, reverse=True):
        if secret:
            text = text.replace(secret, "<REDACTED>")
    if host:
        text = text.replace(host, "rented-host")
    text = re.sub(r"/(?:home/[^/\\\s\"']+|root)/", "~/", text)
    text = re.sub(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+", r"\1<REDACTED>", text)
    text = re.sub(r"\bhf_[A-Za-z0-9]+\b", "<REDACTED>", text)
    text = re.sub(r"(?i)((?:api[_-]?key|inference[_-]?key|admin[_-]?token|access[_-]?token|secret|password)[\"']?\s*[:=]\s*)(\"[^\"]*\"|'[^']*'|[^,\s}\]]+)",
                  r'\1"<REDACTED>"', text)
    return text


def cmd_snapshot(a):
    """Export an allowlist, never raw state, keys, credentials, or database files."""
    root = Path(a.root)
    attempt = root / "attempts" / a.attempt
    dest = root / "evidence" / a.attempt
    secrets = []
    # These are harness-generated inference/admin credentials, not engine keys.
    for credentials in (attempt / "state").glob("*/identity/*credentials*"):
        text = credentials.read_text(errors="replace")
        try:
            doc = json.loads(text)
            secrets.extend(v for k, v in doc.items() if isinstance(v, str) and ("key" in k or "token" in k))
        except ValueError:
            secrets.extend(line.split(":", 1)[1].strip().strip("\"'") for line in text.splitlines() if ":" in line)
    def copy(source, relative):
        if source.is_file() and not source.is_symlink():
            atomic_text(dest / relative, sanitize(source.read_text(errors="replace"), secrets, a.host))
    for source in (attempt / "raw").rglob("*"):
        if source.is_file() and source.suffix in (".log", ".txt", ".tsv", ".json", ".jsonl", ".out", ".yaml"):
            copy(source, Path("raw") / source.relative_to(attempt / "raw"))
    for source in (root / "dep").glob("*.yaml"):
        copy(source, Path("raw/dep") / source.name)
    failures = []
    for role in (attempt / "state").glob("*"):
        for log_root in (role / "logs", role / "state/logs"):
            for source in log_root.rglob("*"):
                copy(source, Path("raw/engine-logs") / role.name / source.relative_to(log_root))
        if role.name not in ("s1", "s2", "srv"):
            continue
        db = role / ("srv.sqlite3" if role.name == "srv" else "server/srv.sqlite3")
        if not db.is_file():
            failures.append(role.name)
            atomic_text(dest / f"raw/journal-{role.name}-error.txt", "expected coordinator database missing\n")
        else:
            try:
                for label, query in (
                    ("events", "SELECT sequence,recorded_at_ms,kind,deployment_id,payload_json FROM management_events ORDER BY sequence"),
                    ("journal", "SELECT recorded_at,state,evidence FROM journal_entries ORDER BY recorded_at"),
                ):
                    entries = rows(db, query)
                    atomic_text(dest / f"raw/{label}-{role.name}.jsonl",
                                sanitize("".join(json.dumps(entry) + "\n" for entry in entries), secrets, a.host))
            except sqlite3.Error as exc:
                failures.append(role.name)
                atomic_text(dest / f"raw/journal-{role.name}-error.txt", str(exc) + "\n")
    for name in ("settings.json", "host-completion.json"):
        copy(attempt / name, Path(name))
    runner_alive = None
    if (attempt / "runner.pid").is_file():
        try:
            pid = int((attempt / "runner.pid").read_text())
            stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            runner_alive = stat[0] != "Z" and stat[19] == (attempt / "runner.ticks").read_text().strip()
        except (OSError, ValueError, IndexError):
            runner_alive = False
    atomic_text(dest / "snapshot.json", json.dumps({"attempt": a.attempt, "copied_at": time.time(),
                                                  "runner_alive": runner_alive, "errors": failures}) + "\n")
    return 1 if failures else 0


def cmd_redact(a):
    for line in sys.stdin:
        print(sanitize(line.rstrip("\n"), host=a.host), flush=True)
    return 0


def cmd_sanitize_tree(a):
    for path in Path(a.root).rglob("*"):
        if path.is_file() and not path.is_symlink() and not path.name.startswith("."):
            text = path.read_text(errors="replace")
            clean = sanitize(text, host=a.host)
            if clean != text:
                atomic_text(path, clean)
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
    s = sub.add_parser("rank-exit")
    s.add_argument("events")
    s.add_argument("--rank", type=int, required=True)
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
    s.add_argument("--only", default="all")
    s.add_argument("--attempts")
    s.add_argument("--host-status")
    s.add_argument("--collection-status")
    s = sub.add_parser("fit-check")
    s.add_argument("log")
    s.add_argument("charge")
    s = sub.add_parser("snapshot")
    s.add_argument("--root", required=True)
    s.add_argument("--attempt", required=True)
    s.add_argument("--host", default="")
    s = sub.add_parser("redact")
    s.add_argument("--host", default="")
    s = sub.add_parser("sanitize-tree")
    s.add_argument("root")
    s.add_argument("--host", default="")
    s = sub.add_parser("find")
    s.add_argument("file")
    s.add_argument("key")
    s = sub.add_parser("engine-args")
    s.add_argument("--engine", required=True, choices=["vllm", "sglang"])
    s.add_argument("log")
    s.add_argument("--argv")
    s.add_argument("--pid", type=int)
    a = p.parse_args()
    return globals()["cmd_" + a.cmd.replace("-", "_")](a)


if __name__ == "__main__":
    sys.exit(main())
