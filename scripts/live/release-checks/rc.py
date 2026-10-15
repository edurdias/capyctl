#!/usr/bin/env python3
"""Requests, reads and verdicts for the 0.1.3 release checks (run.sh, remote.sh).

Standard library only. The role's inference key and admin token are read from
its credentials file (`<state>/identity/credentials`) and never printed or
written; engine keys are never read. The role's ledger is opened read-only.

    rc.py chat      --cred F --endpoint URL --model M [--prompt P] [--max-tokens N] [--salt S] [--out F]
                    one request; prints `HTTP seconds ok|wrong|-` (ok: the default prompt's 42)
    rc.py stream    --cred F --endpoint URL --model M [--prompt P] [--max-tokens N] [--salt S] --out F
    rc.py metrics   --engine vllm|sglang|tensorfold --chat F --stream F      pass|fail<TAB>detail
    rc.py cached    --cred F --endpoint URL --model M --out F [--salt S --salt2 S]
                    the same long prompt twice (under S), then under S2; cached tokens of each
    rc.py cache-eval FILE --expect cached|salt|refused                       pass|fail<TAB>detail
    rc.py load      --cred F --maddr F --name M --out F [--pid P] [--limit S] [--want-max N]
                    polls the management load read while P runs              pass|fail<TAB>detail
    rc.py selection ROLE_LOG [--from-line N]                                  pass|fail<TAB>detail
    rc.py redaction --cred F --marker M [--sglang] LOG...                     pass|fail<TAB>detail
    rc.py tail      --cred F --maddr F --id DEPLOYMENT_ID --marker M --out F  pass|fail<TAB>detail
    rc.py absent    --needle-file F FILE...    pass when no file holds the needle (never printed)
    rc.py facts     LOG... --out F             engine-resolved values from an engine log
    rc.py get       FILE PATH                  a value of a JSON file (a.b[0].c["x.y"]), any depth
    rc.py sql       DB QUERY                   rows of a read-only SELECT, tab-separated
    rc.py ckpt      DIR                        safetensors completeness and n-gram table bytes (JSON)
    rc.py verdict   RESULTS_TSV ESTIMATES_TSV OUT_MD [--settings F]
"""
import argparse
import http.client
import json
import re
import sqlite3
import struct
import sys
import time
from pathlib import Path

DEFAULT_PROMPT = "What is 17+25? Answer with only the number."
# A long, fixed prefix: enough tokens for vLLM's 16-token blocks and SGLang's radix cache.
LONG_PREFIX = " ".join(
    f"Line {i}: the quick brown fox jumps over the lazy dog while the river runs to the sea."
    for i in range(60))


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


def post(endpoint, cred, body, timeout=1800):
    host, port = host_port(endpoint)
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    headers = {"Authorization": "Bearer " + credential(cred, "api_key"), "Content-Type": "application/json"}
    t0 = time.perf_counter()
    conn.request("POST", "/v1/chat/completions", body=json.dumps(body), headers=headers)
    return conn, conn.getresponse(), t0


def admin_get(cred, maddr_file, path, timeout=10):
    address = Path(maddr_file).read_text().strip()
    host, port = address.rsplit(":", 1)
    conn = http.client.HTTPConnection(host, int(port), timeout=timeout)
    conn.request("GET", path, headers={"Authorization": "Bearer " + credential(cred, "admin_token")})
    resp = conn.getresponse()
    raw = resp.read()
    conn.close()
    return resp.status, raw


def body_for(a, stream):
    body = {"model": a.model, "temperature": 0, "max_tokens": a.max_tokens,
            "messages": [{"role": "user", "content": a.prompt or DEFAULT_PROMPT}]}
    if stream:
        body.update(stream=True, stream_options={"include_usage": True})
    if a.salt:
        body["cache_salt"] = a.salt
    return body


def dump(path, obj):
    if path:
        Path(path).write_text(json.dumps(obj, indent=1) + "\n")


def load_json(path):
    """The file as JSON; for captured CLI output with other lines (stderr), the
    last object that starts at the beginning of a line, single-line or pretty-printed."""
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    try:
        return json.loads(text)
    except ValueError:
        pass
    lines = text.splitlines()
    decoder = json.JSONDecoder()
    for i in reversed(range(len(lines))):
        if lines[i].startswith("{"):
            try:
                return decoder.raw_decode("\n".join(lines[i:]))[0]
            except ValueError:
                continue
    return None


# ---------------------------------------------------------------------------
# Requests
# ---------------------------------------------------------------------------

def cmd_chat(a):
    try:
        conn, resp, t0 = post(a.endpoint, a.cred, body_for(a, False))
        raw = resp.read()
        conn.close()
        status = resp.status
    except OSError as e:
        print(f"000 0.0 - ({type(e).__name__})")
        return
    took = time.perf_counter() - t0
    try:
        d = json.loads(raw)
    except ValueError:
        d = {"raw": raw[:2000].decode(errors="replace")}
    d = d if isinstance(d, dict) else {"body": d}
    d["_http_status"], d["_total_s"] = status, round(took, 3)
    dump(a.out, d)
    msg = ((d.get("choices") or [{}])[0] or {}).get("message") or {}
    content = msg.get("content") or ""
    if a.prompt:
        verdict = "ok" if status == 200 and (content or msg.get("reasoning_content") or msg.get("reasoning")) else "-"
    else:
        verdict = "ok" if "42" in content else "wrong"
    print(f"{status} {took:.1f} {verdict}")


def cmd_stream(a):
    out = {"http_status": None, "chunks": 0, "content_chars": 0, "usage": None, "done": False,
           "metrics_comment": False, "comment_before_done": False, "metrics": None,
           "first_token_s": None, "finish_reason": None, "error": None}
    try:
        conn, resp, t0 = post(a.endpoint, a.cred, body_for(a, True))
    except OSError as e:
        out["error"] = type(e).__name__
        dump(a.out, out)
        print("fail\tno connection")
        return
    out["http_status"] = resp.status
    if resp.status != 200:
        raw = resp.read()
        try:
            out["error"] = json.loads(raw)
        except ValueError:
            out["error"] = raw[:500].decode(errors="replace")
    else:
        for line in resp:
            line = line.strip()
            if line.startswith(b": x-capyctl-metrics "):
                out["metrics_comment"] = True
                out["comment_before_done"] = not out["done"]
                try:
                    out["metrics"] = json.loads(line[len(b": x-capyctl-metrics "):])
                except ValueError:
                    out["metrics"] = "unparsable"
                continue
            if not line.startswith(b"data:"):
                continue
            data = line[5:].strip()
            if data == b"[DONE]":
                out["done"] = True
                break
            try:
                obj = json.loads(data)
            except ValueError:
                continue
            out["chunks"] += 1
            if obj.get("error"):
                out["error"] = obj["error"]
            if obj.get("usage"):
                out["usage"] = obj["usage"]
            for ch in obj.get("choices") or []:
                d = ch.get("delta") or {}
                text = (d.get("content") or "") + (d.get("reasoning_content") or d.get("reasoning") or "")
                if text and out["first_token_s"] is None:
                    out["first_token_s"] = round(time.perf_counter() - t0, 3)
                out["content_chars"] += len(text)
                if ch.get("finish_reason"):
                    out["finish_reason"] = ch["finish_reason"]
    conn.close()
    out["total_s"] = round(time.perf_counter() - t0, 3)
    dump(a.out, out)
    print(json.dumps({k: out[k] for k in ("http_status", "done", "metrics_comment", "comment_before_done")}))


FIGURES = ("ttft_ms", "queue_ms", "prefill_ms", "decode_tokens_per_second", "cached_tokens")


def sources(metrics):
    return {k: (v or {}).get("source") for k, v in (metrics or {}).items()
            if k in FIGURES and isinstance(v, dict)}


def cmd_metrics(a):
    """Per-request engine metrics (docs/changes/per-request-engine-metrics.md,
    verify-tensorfold-0.6.6.md): the figures each engine must report itself."""
    chat, stream = load_json(a.chat) or {}, load_json(a.stream) or {}
    m = ((chat.get("capyctl") or {}).get("metrics")) or {}
    src, ssrc = sources(m), sources(stream.get("metrics") if isinstance(stream.get("metrics"), dict) else {})
    need_engine = {"vllm": ("ttft_ms", "prefill_ms"), "sglang": (),
                   "tensorfold": ("prefill_ms",)}[a.engine]
    need_router = {"vllm": (), "sglang": ("ttft_ms",), "tensorfold": ("ttft_ms",)}[a.engine]
    problems = []
    if chat.get("_http_status") != 200:
        problems.append(f"chat HTTP {chat.get('_http_status')}")
    if not m:
        problems.append("no capyctl.metrics in the answer")
    for k in need_engine:
        if src.get(k) != "engine":
            problems.append(f"{k} source {src.get(k)}, expected engine")
    for k in need_router:
        if src.get(k) != "router":
            problems.append(f"{k} source {src.get(k)}, expected router")
    if stream.get("http_status") != 200 or not stream.get("done"):
        problems.append(f"stream HTTP {stream.get('http_status')}, done {stream.get('done')}")
    if not stream.get("metrics_comment"):
        problems.append("no `: x-capyctl-metrics` comment in the stream")
    elif not stream.get("comment_before_done"):
        problems.append("the metrics comment came after [DONE]")
    if not stream.get("usage"):
        problems.append("no usage chunk with include_usage")
    for k in need_engine:
        if ssrc.get(k) != "engine":
            problems.append(f"stream {k} source {ssrc.get(k)}, expected engine")
    detail = f"answer {src or 'none'}; stream comment {ssrc or 'none'}"
    print(("fail\t" + "; ".join(problems) + "; " if problems else "pass\t") + detail)


def one_cached(a, salt):
    body = {"model": a.model, "temperature": 0, "max_tokens": 16,
            "messages": [{"role": "user", "content": LONG_PREFIX + "\nHow many lines are above? Answer briefly."}]}
    if salt:
        body["cache_salt"] = salt
    conn, resp, _ = post(a.endpoint, a.cred, body)
    raw = resp.read()
    conn.close()
    try:
        d = json.loads(raw)
    except ValueError:
        d = {}
    usage = d.get("usage") or {}
    details = usage.get("prompt_tokens_details") or {}
    metric = (((d.get("capyctl") or {}).get("metrics") or {}).get("cached_tokens")) or {}
    return {"http_status": resp.status, "code": d.get("code"), "prompt_tokens": usage.get("prompt_tokens"),
            "usage_cached_tokens": details.get("cached_tokens"), "metric_cached_tokens": metric.get("value"),
            "metric_source": metric.get("source")}


def cmd_cached(a):
    runs = {"first": one_cached(a, a.salt), "repeat": one_cached(a, a.salt)}
    if a.salt2:
        runs["other_salt"] = one_cached(a, a.salt2)
    dump(a.out, {"salt": bool(a.salt), "runs": runs})
    print(json.dumps({k: (v["http_status"], v["usage_cached_tokens"]) for k, v in runs.items()}))


def cmd_cache_eval(a):
    d = load_json(a.file) or {}
    runs = d.get("runs") or {}
    first, repeat, other = runs.get("first") or {}, runs.get("repeat") or {}, runs.get("other_salt") or {}
    cached = repeat.get("usage_cached_tokens") or 0
    detail = (f"HTTP {first.get('http_status')}/{repeat.get('http_status')}"
              f"{'/' + str(other.get('http_status')) if other else ''}; prompt {repeat.get('prompt_tokens')} tokens; "
              f"cached on the repeat {repeat.get('usage_cached_tokens')} (metric {repeat.get('metric_cached_tokens')}, "
              f"source {repeat.get('metric_source')})")
    if a.expect == "refused":
        ok = first.get("http_status") == 400 and first.get("code") == "cache_salt_unsupported"
        detail = f"HTTP {first.get('http_status')} code {first.get('code')}"
    elif a.expect == "cached":
        ok = first.get("http_status") == 200 and cached > 0 and repeat.get("metric_source") == "engine"
    else:
        other_cached = other.get("usage_cached_tokens")
        detail += f"; under a second salt {other_cached}"
        ok = (first.get("http_status") == 200 and cached > 0 and other.get("http_status") == 200
              and (other_cached or 0) == 0)
    print(("pass" if ok else "fail") + "\t" + detail)


# ---------------------------------------------------------------------------
# Management reads
# ---------------------------------------------------------------------------

def alive(pid):
    """A finished background request lingers as a zombie until `wait`: exited."""
    try:
        with open(f"/proc/{int(pid)}/stat") as stat:
            return stat.read().rsplit(")", 1)[1].split()[0] != "Z"
    except (OSError, ValueError):
        return False


def cmd_load(a):
    """docs/changes/standalone-load-reports.md: a fresh engine sample while a request
    runs; with --want-max, `max_running` from the engine equal to that figure."""
    reads = fresh = 0
    peak, maxes, errors = None, set(), []
    deadline = time.monotonic() + a.limit
    with open(a.out, "a") as log:
        while True:
            time.sleep(0.5)
            try:
                status, raw = admin_get(a.cred, a.maddr, "/management/v1/metrics/load")
                body = json.loads(raw) if status == 200 else None
                if status != 200:
                    errors.append(f"HTTP {status}")
            except (OSError, ValueError) as e:
                errors.append(type(e).__name__)
                body = None
            if body is not None:
                reads += 1
                for dep in body.get("deployments") or []:
                    if dep.get("name") != a.name:
                        continue
                    for inst in dep.get("instances") or []:
                        sample = inst.get("sample") or {}
                        engine = sample.get("engine") or {}
                        mr = inst.get("max_running") or {}
                        log.write(json.dumps({"t": round(time.time(), 3), "instance": inst.get("index"),
                                              "router_in_flight": inst.get("router_in_flight"),
                                              "max_running": mr, "sample": inst.get("sample")}) + "\n")
                        if mr:
                            maxes.add((mr.get("count"), mr.get("source")))
                        if sample.get("fresh") and engine.get("running") is not None \
                                and engine.get("waiting") is not None:
                            fresh += 1
                            peak = max(peak or 0, engine["running"])
            if (a.pid and not alive(a.pid)) or time.monotonic() > deadline:
                break
    engine_max = sorted(c for c, s in maxes if s == "engine")
    detail = (f"{reads} load reads, {fresh} with a fresh engine sample, engine running up to {peak}, "
              f"max_running {sorted(maxes, key=str) or 'none'}")
    if errors:
        detail += f", {len(errors)} failed reads ({errors[-1]})"
    ok = fresh > 0 and bool(peak) and bool(engine_max)
    if a.want_max is not None:
        ok = ok and engine_max == [a.want_max]
        detail += f"; engine log resolved max_running_requests={a.want_max}"
    print(("pass" if ok else "fail") + "\t" + detail)


def cmd_selection(a):
    """The router's `router_selection` events (crates/capyctl-router/src/balance.rs)
    in the role's JSON log, from line N on: some candidate scored on engine load."""
    seen, sources_seen = 0, {}
    lines = Path(a.log).read_text(errors="replace").splitlines()[a.from_line:]
    for line in lines:
        start = line.find("{")
        if start < 0 or "router_selection" not in line:
            continue
        try:
            ev = json.loads(line[start:])
        except ValueError:
            continue
        if ev.get("event") != "router_selection":
            continue
        seen += 1
        for c in ev.get("candidates") or []:
            s = c.get("load_source")
            sources_seen[s] = sources_seen.get(s, 0) + 1
    ok = sources_seen.get("engine", 0) > 0
    print(("pass" if ok else "fail") + f"\t{seen} router_selection events, candidate load_source counts {sources_seen}")


CRED_SHAPES = [
    re.compile(r"""(?i)\b(?:api[_-]?key|admin[_-]?api[_-]?key|token|secret|password)\b['"]?\s*[=:]\s*['"]?(?!<redacted>|none\b|null\b|''|"")([A-Za-z0-9_\-+/=.]{12,})"""),
    re.compile(r"\bhf_[A-Za-z0-9]{20,}\b"),
    re.compile(r"(?i)\bbearer\s+(?!<redacted>)[A-Za-z0-9_\-.=]{16,}"),
]


def cmd_redaction(a):
    """docs/changes/engine-log-redaction.md: no credential and no prompt text in the
    engine log. The per-launch engine key is never read, so the check is by rule
    (credential-shaped assignments, `hf_` tokens, bearer values), by value for the
    role's own key and token, and by value for the driven prompt's marker."""
    secrets = [credential(a.cred, "api_key"), credential(a.cred, "admin_token")]
    text = ""
    for p in a.logs:
        try:
            text += Path(p).read_text(errors="replace")
        except OSError:
            pass
    problems = []
    if not text:
        problems.append("no engine log")
    if a.marker in text:
        problems.append("the prompt's marker is in the log")
    if any(s and s in text for s in secrets):
        problems.append("the role's own key or token is in the log")
    shapes = sum(len(r.findall(text)) for r in CRED_SHAPES)
    if shapes:
        problems.append(f"{shapes} credential-shaped values")
    redacted = text.count("<redacted>")
    lines = text.count("\n")
    detail = f"{lines} log lines, {redacted} `<redacted>`"
    if a.sglang:
        args_lines = [ln for ln in text.splitlines() if "server_args=" in ln]
        warnings = sum(1 for ln in text.splitlines() if "warn" in ln.lower())
        key_vals = re.findall(r"\b(?:api_key|admin_api_key)=('[^']*'|None)", "\n".join(args_lines))
        detail += f", server_args line {'present' if args_lines else 'absent'} (keys {sorted(set(key_vals))}), {warnings} warning lines"
        if not args_lines:
            problems.append("no info-level server_args line: SGLang's info output is not kept")
        if any(v not in ("None", "'<redacted>'") for v in key_vals):
            problems.append("a server_args key is not redacted")
    print(("fail\t" + "; ".join(problems) + "; " if problems else "pass\t") + detail)


def cmd_tail(a):
    status, raw = admin_get(a.cred, a.maddr, f"/management/v1/deployments/{a.id}/engine-log?instance=0&kib=64")
    try:
        d = json.loads(raw)
    except ValueError:
        d = {}
    text = d.get("text") or ""
    secrets = [credential(a.cred, "api_key"), credential(a.cred, "admin_token")]
    dump(a.out, {"http_status": status, "kib": d.get("kib"), "truncated": d.get("truncated"),
                 "incarnation": d.get("incarnation"), "lines": text.count("\n"),
                 "error": d.get("error")})
    problems = []
    if status != 200 or not text:
        problems.append(f"HTTP {status}, {len(text)} bytes")
    if a.marker in text:
        problems.append("the prompt's marker is in the tail")
    if any(s and s in text for s in secrets):
        problems.append("a role credential is in the tail")
    detail = f"HTTP {status}, {text.count(chr(10))} lines, truncated {d.get('truncated')}"
    print(("fail\t" + "; ".join(problems) + "; " if problems else "pass\t") + detail)


def cmd_absent(a):
    needle = Path(a.needle_file).read_text().strip()
    hits = []
    for p in a.files:
        try:
            if needle and needle in Path(p).read_text(errors="replace"):
                hits.append(Path(p).name)
        except OSError:
            pass
    print(("fail\tfound in " + ", ".join(hits)) if hits else f"pass\tnot in {len(a.files)} files")


# ---------------------------------------------------------------------------
# Files
# ---------------------------------------------------------------------------

FACT_PATTERNS = {
    "max_total_num_tokens": r"max_total_num_tokens=(\d+)",
    "max_running_requests": r"\bmax_running_requests=(\d+)",
    "context_len": r"\bcontext_len=(\d+)",
    "mem_fraction_static": r"\bmem_fraction_static=([\d.]+)",
    "max_mamba_cache_size": r"\bmax_mamba_cache_size[=:]\s*(\d+)",
    "speculative_algorithm": r"\bspeculative_algorithm='?([A-Za-z0-9_]+)",
    "speculative_draft_model_path": r"\bspeculative_draft_model_path='([^']+)'",
    "kv_tokens_allocated": r"KV Cache is allocated\. #tokens: (\d+)",
}


def cmd_facts(a):
    """The last match of each pattern: SGLang logs its server_args first and the
    resolved figures after the pools are sized."""
    facts, oom = {}, 0
    for path in a.logs:
        try:
            text = Path(path).read_text(errors="replace")
        except OSError:
            continue
        for line in text.splitlines():
            for key, pat in FACT_PATTERNS.items():
                m = re.search(pat, line)
                if m:
                    facts[key] = m.group(1)
            if "out of memory" in line.lower():
                oom += 1
    facts["out_of_memory_lines"] = oom
    dump(a.out, facts)
    print(json.dumps(facts))


PATH_TOKEN = re.compile(r'\.?(?:(\w+)|\[(\d+)\]|\["([^"]+)"\])')


def cmd_get(a):
    """The value at a dotted path, searched from every object in the file, so the
    path may start at any depth; empty when absent."""
    parts = [m.groups() for m in PATH_TOKEN.finditer(a.path)]

    def follow(d):
        for key, index, quoted in parts:
            if index is not None:
                if not isinstance(d, list) or int(index) >= len(d):
                    return None
                d = d[int(index)]
            else:
                k = key if key is not None else quoted
                if not isinstance(d, dict) or k not in d:
                    return None
                d = d[k]
        return d

    def walk(d):
        yield d
        for v in (d.values() if isinstance(d, dict) else d if isinstance(d, list) else ()):
            yield from walk(v)

    data = load_json(a.file)
    for node in walk(data) if data is not None else ():
        v = follow(node)
        if v is not None:
            print(v if not isinstance(v, (dict, list, bool)) else json.dumps(v))
            return


def cmd_sql(a):
    """AGENTS.md: the ledger is read-only; SELECT only, opened mode=ro."""
    if not re.match(r"(?is)^\s*select\b", a.query) or ";" in a.query.strip().rstrip(";"):
        raise SystemExit("only one SELECT")
    conn = sqlite3.connect(f"file:{a.db}?mode=ro", uri=True, timeout=10)
    try:
        for row in conn.execute(a.query):
            print("\t".join("" if v is None else str(v) for v in row))
    finally:
        conn.close()


def cmd_ckpt(a):
    """Whether a safetensors checkpoint is complete (every shard the index names is
    present, each file as long as its header says) and its n-gram table bytes
    (`ple_embedding.ngram_embedding.shard_<n>` tensors, docs/changes/disk-backed-weights.md)."""
    root = Path(a.dir)
    out = {"dir_exists": root.is_dir(), "complete": False, "files": 0, "bytes": 0,
           "table_bytes": 0, "missing": [], "short": [], "format": "unknown", "gib": 0.0, "table_gib": 0.0,
           "missing_count": 0}
    if not root.is_dir():
        print(json.dumps(out))
        return
    index = load_json(root / "model.safetensors.index.json")
    names = sorted(set((index or {}).get("weight_map", {}).values())) if index else \
        sorted(p.name for p in root.glob("*.safetensors"))
    for name in names:
        p = root / name
        if not p.is_file():
            out["missing"].append(name)
            continue
        size = p.stat().st_size
        out["files"] += 1
        out["bytes"] += size
        try:
            with open(p, "rb") as f:
                n = struct.unpack("<Q", f.read(8))[0]
                header = json.loads(f.read(n))
        except (OSError, ValueError, struct.error):
            out["short"].append(name)
            continue
        end = max((v["data_offsets"][1] for k, v in header.items() if k != "__metadata__"), default=0)
        if size < 8 + n + end:
            out["short"].append(name)
        out["table_bytes"] += sum(v["data_offsets"][1] - v["data_offsets"][0] for k, v in header.items()
                                  if k != "__metadata__" and "ngram_embedding.shard_" in k)
    cfg = load_json(root / "config.json") or {}
    quant = load_json(root / "hf_quant_config.json") or {}
    qc = cfg.get("quantization_config") or cfg.get("quantization") or {}
    if quant or (isinstance(qc, dict) and "modelopt" in json.dumps(qc).lower()):
        out["format"] = "modelopt"
    elif isinstance(qc, dict) and "group_size" in qc and "bits" in qc:
        out["format"] = "mlx"
    out["complete"] = bool(names) and not out["missing"] and not out["short"]
    out["missing_count"] = len(out["missing"])
    out["gib"] = round(out["bytes"] / 2**30, 1)
    out["table_gib"] = round(out["table_bytes"] / 2**30, 1)
    print(json.dumps(out))


# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------

TITLES = {
    "B1": "vLLM loader default by memory shape (lazy, deep, 3 cycles)",
    "B2": "Single-host wake canary (3 GB10 wakes)",
    "B3": "Per-request engine metrics",
    "B4": "Parked-growth stop runs (vLLM eager)",
    "B5": "Engine log redaction and management tail",
    "B6": "Standalone load reports (SGLang)",
    "B7": "Admission figures",
    "B8": "Explicit activation",
    "B9": "SGLang static pool on unified memory",
    "B10": "MAX_JOBS on unified memory + disk-backed weights (Flash-Next)",
    "B11": "Digest readiness (fetched provenance)",
    "B12": "Draft model source",
    "B13": "Source extensions (plain HTTP, secret URL)",
    "B14": "cache_salt passthrough and refusal",
    "B15": "SGLang load read: engine max_running",
    "B16": "Unified margin (vLLM gpt-oss-120b)",
    "B17": "Container readiness",
    "B18": "Recipe comparison: not this driver (capyctl-recipes comparisons/qwen3.8-27b-sglang-dflash2-vs-mtp-eagle-gb10/run.sh)",
}


def cmd_verdict(a):
    rows = []
    for line in Path(a.results).read_text().splitlines() if Path(a.results).exists() else []:
        parts = line.split("\t")
        if len(parts) >= 4:
            rows.append(parts[:4])
    est = {}
    for line in Path(a.estimates).read_text().splitlines() if Path(a.estimates).exists() else []:
        parts = line.split("\t")
        if len(parts) >= 2:
            est[parts[0]] = parts[1]
    settings = load_json(a.settings) if a.settings else None
    by_id = {}
    for cid, scope, status, detail in rows:
        by_id.setdefault(cid, []).append((scope, status.upper(), detail))
    lines = ["# 0.1.3 release live checks: verdict", ""]
    if settings:
        lines += [f"- {k}: `{v}`" for k, v in settings.items()] + [""]
    lines += ["| ID | Check | Result | Sub-checks | Estimate |", "|---|---|---|---|---|"]
    order = sorted(set(TITLES) | set(by_id), key=lambda s: int(re.sub(r"\D", "", s) or 0))
    for cid in order:
        subs = by_id.get(cid)
        if not subs:
            result, summary = ("n/a", "separate driver") if cid == "B18" else \
                ("NOT RUN", "not selected, or the run ended first")
        else:
            states = [s for _, s, _ in subs]
            result = "FAIL" if "FAIL" in states else "PASS" if "PASS" in states else "SKIP"
            summary = "; ".join(f"{sc}: {st}" for sc, st, _ in subs)
        lines.append(f"| {cid} | {TITLES.get(cid, '')} | **{result}** | {summary} | {est.get(cid, '')} |")
    table_end = len(lines)
    lines += ["", "## Evidence", "", "| ID | Scope | Result | Detail |", "|---|---|---|---|"]
    for cid, scope, status, detail in rows:
        lines.append(f"| {cid} | {scope} | {status.upper()} | {detail.replace('|', '/')} |")
    lines += ["", "CPU and Fake-engine tests are not qualification; these rows are the live evidence.", ""]
    Path(a.out).write_text("\n".join(lines))
    print("\n".join(lines[:table_end]))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("chat", "stream", "cached"):
        s = sub.add_parser(name)
        s.add_argument("--cred", required=True)
        s.add_argument("--endpoint", required=True)
        s.add_argument("--model", required=True)
        s.add_argument("--out", required=name != "chat")
        s.add_argument("--prompt")
        s.add_argument("--max-tokens", type=int, default=2048)
        s.add_argument("--salt")
        s.add_argument("--salt2")
    s = sub.add_parser("metrics")
    s.add_argument("--engine", required=True, choices=("vllm", "sglang", "tensorfold"))
    s.add_argument("--chat", required=True)
    s.add_argument("--stream", required=True)
    s = sub.add_parser("cache-eval")
    s.add_argument("file")
    s.add_argument("--expect", required=True, choices=("cached", "salt", "refused"))
    s = sub.add_parser("load")
    s.add_argument("--cred", required=True)
    s.add_argument("--maddr", required=True)
    s.add_argument("--name", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--pid")
    s.add_argument("--limit", type=float, default=60.0)
    s.add_argument("--want-max", type=int)
    s = sub.add_parser("selection")
    s.add_argument("log")
    s.add_argument("--from-line", type=int, default=0)
    s = sub.add_parser("redaction")
    s.add_argument("--cred", required=True)
    s.add_argument("--marker", required=True)
    s.add_argument("--sglang", action="store_true")
    s.add_argument("logs", nargs="*")
    s = sub.add_parser("tail")
    s.add_argument("--cred", required=True)
    s.add_argument("--maddr", required=True)
    s.add_argument("--id", required=True)
    s.add_argument("--marker", required=True)
    s.add_argument("--out", required=True)
    s = sub.add_parser("absent")
    s.add_argument("--needle-file", required=True)
    s.add_argument("files", nargs="*")
    s = sub.add_parser("facts")
    s.add_argument("logs", nargs="+")
    s.add_argument("--out", required=True)
    s = sub.add_parser("get")
    s.add_argument("file")
    s.add_argument("path")
    s = sub.add_parser("sql")
    s.add_argument("db")
    s.add_argument("query")
    s = sub.add_parser("ckpt")
    s.add_argument("dir")
    s = sub.add_parser("verdict")
    s.add_argument("results")
    s.add_argument("estimates")
    s.add_argument("out")
    s.add_argument("--settings")
    a = p.parse_args()
    {"chat": cmd_chat, "stream": cmd_stream, "metrics": cmd_metrics, "cached": cmd_cached,
     "cache-eval": cmd_cache_eval, "load": cmd_load, "selection": cmd_selection,
     "redaction": cmd_redaction, "tail": cmd_tail, "absent": cmd_absent, "facts": cmd_facts,
     "get": cmd_get, "sql": cmd_sql, "ckpt": cmd_ckpt, "verdict": cmd_verdict}[a.cmd](a)


if __name__ == "__main__":
    main()
