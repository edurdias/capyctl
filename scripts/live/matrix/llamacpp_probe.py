#!/usr/bin/env python3
"""Probes for the llama.cpp rows LC1-LC6 (scripts/live/matrix/rows/LC*.sh).

Runs on the host, beside a standalone role started with its own state
directory (--dir). The inference key and the admin token are read from
<dir>/identity/credentials and never printed. Every subcommand prints one JSON
document per line on standard output; the row keeps it as step output.

  chat     one chat request through the standalone router (plain, --stream,
           --tools), with the shapes the design note's live checks ask for
  engine   llama-server's own /metrics, /slots and /props on its private port
  load     the management load read (/management/v1/metrics/load)
  hangup   a stream cut after N chunks (or before the first, mid-prefill), then
           how long llama-server's requests_processing takes to reach 0
  streams  N concurrent streams; while they run, the engine's /metrics, /slots
           and the management load read
  sample   memory samples until killed: /proc/meminfo, the llama-server
           process's resident set, nvidia-smi's compute apps and how much of the
           GGUF file is in the page cache (mincore)
  summary  peak and steady figures from a sample file
"""

import argparse
import ctypes
import ctypes.util
import json
import mmap
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

ROUTER = "http://127.0.0.1:8443"
MANAGEMENT = "http://127.0.0.1:7443"
PROMPT = "What is 17+25? Answer with only the number."
TOOL = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Current weather for a city.",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    },
}


def credentials(state_dir):
    creds = {}
    with open(os.path.join(state_dir, "identity", "credentials")) as f:
        for line in f:
            key, _, value = line.partition(": ")
            if value:
                creds[key.strip()] = value.strip()
    return creds


def emit(doc):
    print(json.dumps(doc, sort_keys=True), flush=True)


def llama_pids():
    pids = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as f:
                argv = f.read().split(b"\0")
        except OSError:
            continue
        if argv and os.path.basename(argv[0].decode(errors="replace")) == "llama-server":
            pids.append((int(entry), [a.decode(errors="replace") for a in argv if a]))
    return pids


def engine_port():
    for _, argv in llama_pids():
        if "--port" in argv:
            return int(argv[argv.index("--port") + 1])
    return None


def http(url, body=None, headers=None, timeout=300):
    req = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json", **(headers or {})})
    return urllib.request.urlopen(req, timeout=timeout)


def chat_body(args, prompt=None, stream=False):
    body = {"model": args.route, "messages": [{"role": "user", "content": prompt or args.prompt}],
            "max_tokens": args.max_tokens, "temperature": 0, "stream": stream}
    if args.tools:
        body["messages"] = [{"role": "user", "content": "What is the weather in Lisbon? Use the tool."}]
        body["tools"] = [TOOL]
    if stream and args.usage:
        body["stream_options"] = {"include_usage": True}
    return body


def cmd_chat(args):
    key = credentials(args.dir)["api_key"]
    auth = {"Authorization": f"Bearer {key}"}
    t0 = time.monotonic()
    out = {"route": args.route, "stream": args.stream, "tools": args.tools}
    try:
        resp = http(f"{ROUTER}/v1/chat/completions", chat_body(args, stream=args.stream), auth, args.timeout)
    except urllib.error.HTTPError as e:
        out.update(status=e.code, error=e.read(2000).decode(errors="replace"), ok=False)
        emit(out)
        return 1
    out["status"] = resp.status
    out["x_capyctl_headers"] = sorted(h for h in resp.headers if h.lower().startswith("x-capyctl"))
    if not args.stream:
        doc = json.load(resp)
        msg = doc["choices"][0]["message"]
        out.update(
            ms=round((time.monotonic() - t0) * 1000),
            message_keys=sorted(msg),
            content_tail=(msg.get("content") or "")[-160:],
            reasoning=bool(msg.get("reasoning_content")),
            tool_calls=[(c["function"]["name"], c["function"]["arguments"]) for c in msg.get("tool_calls") or []],
            finish_reason=doc["choices"][0].get("finish_reason"),
            top_keys=sorted(doc),
            timings=doc.get("timings"),
            capyctl_metrics=(doc.get("capyctl") or {}).get("metrics"),
            usage=doc.get("usage"),
            system_fingerprint=doc.get("system_fingerprint"),
        )
        text = msg.get("content") or ""
    else:
        deltas, comments, chunks, last, first_ms, text, tool_names = set(), [], 0, None, None, "", set()
        for raw in resp:
            line = raw.decode(errors="replace").rstrip("\r\n")
            if line.startswith(":"):
                comments.append(line[:400])
                continue
            if not line.startswith("data: ") or line == "data: [DONE]":
                continue
            chunk = json.loads(line[6:])
            chunks += 1
            last = chunk
            for choice in chunk.get("choices") or []:
                delta = choice.get("delta") or {}
                deltas.update(k for k, v in delta.items() if v not in (None, "", []))
                text += delta.get("content") or ""
                for call in delta.get("tool_calls") or []:
                    name = (call.get("function") or {}).get("name")
                    if name:
                        tool_names.add(name)
                if first_ms is None and (delta.get("content") or delta.get("reasoning_content") or delta.get("tool_calls")):
                    first_ms = round((time.monotonic() - t0) * 1000)
        out.update(
            ms=round((time.monotonic() - t0) * 1000), first_token_ms=first_ms, chunks=chunks,
            delta_fields=sorted(deltas), tool_names=sorted(tool_names), content_tail=text[-160:],
            last_chunk_keys=sorted(last or {}), last_chunk_timings=(last or {}).get("timings"),
            last_chunk_usage=(last or {}).get("usage"),
            capyctl_comments=[c for c in comments if "capyctl" in c],
        )
    if args.expect is not None:
        out["ok"] = args.expect in text
    elif args.tools:
        out["ok"] = bool(out.get("tool_calls") or out.get("tool_names"))
    else:
        out["ok"] = out["status"] == 200
    emit(out)
    return 0 if out["ok"] else 1


def engine_read(port):
    base = f"http://127.0.0.1:{port}"
    doc = {"port": port}
    try:
        text = http(f"{base}/metrics", timeout=10).read().decode()
        doc["metrics"] = {m.group(1): float(m.group(2)) for m in re.finditer(r"^(llamacpp:\w+)\s+(\S+)$", text, re.M)}
    except (urllib.error.URLError, OSError) as e:
        doc["metrics_error"] = str(e)
    try:
        slots = json.load(http(f"{base}/slots", timeout=10))
        doc["slots"] = [{k: s.get(k) for k in ("id", "n_ctx", "is_processing")} for s in slots]
    except (urllib.error.URLError, OSError, ValueError) as e:
        doc["slots_error"] = str(e)
    try:
        props = json.load(http(f"{base}/props", timeout=10))
        doc["props"] = {k: props.get(k) for k in ("build_info", "total_slots", "model_alias")}
    except (urllib.error.URLError, OSError, ValueError) as e:
        doc["props_error"] = str(e)
    return doc


def cmd_engine(args):
    port = engine_port()
    if port is None:
        emit({"error": "no llama-server process"})
        return 1
    doc = engine_read(port)
    doc["argv"] = [argv for _, argv in llama_pids()]
    emit(doc)
    return 0


def load_read(state_dir):
    token = credentials(state_dir).get("admin_token")
    if not token:
        return {"error": "no admin_token in the standalone credentials"}
    try:
        return json.load(http(f"{MANAGEMENT}/management/v1/metrics/load", headers={"Authorization": f"Bearer {token}"}, timeout=10))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"error": str(e)}


def cmd_load(args):
    emit(load_read(args.dir))
    return 0


def processing(port):
    return engine_read(port).get("metrics", {}).get("llamacpp:requests_processing")


def cmd_hangup(args):
    key = credentials(args.dir)["api_key"]
    port = engine_port()
    if port is None:
        emit({"error": "no llama-server process"})
        return 1
    prompt = args.prompt
    if args.long_prompt:
        prompt = " ".join(f"Item {i}: the quick brown fox jumps over the lazy dog." for i in range(args.long_prompt)) + " Summarise."
    resp = http(f"{ROUTER}/v1/chat/completions", chat_body(args, prompt, stream=True), {"Authorization": f"Bearer {key}"}, args.timeout)
    chunks = 0
    if args.after_chunks:
        for raw in resp:
            if raw.startswith(b"data: "):
                chunks += 1
                if chunks >= args.after_chunks:
                    break
    else:
        time.sleep(args.wait_before_cut)
    busy_before = processing(port)
    resp.close()
    cut = time.monotonic()
    zero_ms = None
    while time.monotonic() - cut < args.bound:
        if processing(port) == 0:
            zero_ms = round((time.monotonic() - cut) * 1000)
            break
        time.sleep(0.05)
    doc = {"chunks_before_cut": chunks, "processing_before_cut": busy_before, "zero_after_ms": zero_ms,
           "bound_s": args.bound, "ok": zero_ms is not None}
    emit(doc)
    return 0 if doc["ok"] else 1


def cmd_streams(args):
    key = credentials(args.dir)["api_key"]
    results = []

    def one(i):
        try:
            resp = http(f"{ROUTER}/v1/chat/completions",
                        chat_body(args, f"Write a {args.max_tokens}-word story about the number {i}.", stream=True),
                        {"Authorization": f"Bearer {key}"}, args.timeout)
            n = sum(1 for raw in resp if raw.startswith(b"data: "))
            results.append({"i": i, "status": resp.status, "chunks": n})
        except (urllib.error.URLError, OSError) as e:
            results.append({"i": i, "error": str(e)})

    threads = [threading.Thread(target=one, args=(i,)) for i in range(args.n)]
    for t in threads:
        t.start()
    time.sleep(args.read_after)
    port = engine_port()
    during = {"engine": engine_read(port) if port else None, "load": load_read(args.dir)}
    for t in threads:
        t.join()
    ok = len(results) == args.n and all(r.get("status") == 200 for r in results)
    emit({"during": during, "streams": sorted(results, key=lambda r: r["i"]), "ok": ok})
    return 0 if ok else 1


class Resident:
    """Bytes of one file in the page cache: mincore over a read-only shared
    mapping of it (the mapping itself reads nothing)."""

    def __init__(self, path):
        self.path = path
        self.libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
        self.libc.mmap.restype = ctypes.c_void_p
        self.libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
        self.libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        self.libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]

    def bytes(self):
        try:
            fd = os.open(self.path, os.O_RDONLY)
        except OSError:
            return None
        try:
            size = os.fstat(fd).st_size
            if size == 0:
                return 0
            addr = self.libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
            if addr in (None, ctypes.c_void_p(-1).value):
                return None
            try:
                page = mmap.PAGESIZE
                vec = (ctypes.c_ubyte * ((size + page - 1) // page))()
                if self.libc.mincore(addr, size, vec) != 0:
                    return None
                return sum(v & 1 for v in vec) * page
            finally:
                self.libc.munmap(addr, size)
        finally:
            os.close(fd)


def meminfo():
    out = {}
    with open("/proc/meminfo") as f:
        for line in f:
            key, value = line.split(":", 1)
            if key in ("MemTotal", "MemFree", "MemAvailable", "Cached", "Buffers", "Shmem", "Mapped", "AnonPages", "Unevictable", "Mlocked"):
                out[key + "_kB"] = int(value.split()[0])
    return out


def proc_status(pid):
    out = {}
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                key, _, value = line.partition(":")
                if key in ("VmRSS", "VmHWM", "RssAnon", "RssFile", "RssShmem"):
                    out[key + "_kB"] = int(value.split()[0])
    except OSError:
        pass
    return out


def gpu_apps():
    try:
        text = subprocess.run(["nvidia-smi", "--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"],
                              capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    apps = []
    for line in text.splitlines():
        parts = [p.strip() for p in line.split(",")]
        if len(parts) == 2:
            apps.append({"pid": int(parts[0]), "used_MiB": parts[1]})
    return apps


def cmd_sample(args):
    resident = Resident(args.gguf)
    with open(args.out, "a") as out:
        while True:
            pids = [pid for pid, _ in llama_pids()]
            doc = {"t": round(time.time(), 3), "label": args.label, **meminfo(), "pids": pids,
                   "proc": {str(p): proc_status(p) for p in pids}, "gpu_apps": gpu_apps(),
                   "gguf_page_cache_bytes": resident.bytes()}
            out.write(json.dumps(doc) + "\n")
            out.flush()
            time.sleep(args.interval)


def cmd_summary(args):
    rows = [json.loads(line) for line in open(args.samples) if line.strip()]
    if not rows:
        emit({"error": "no samples"})
        return 1
    base = rows[0]
    up = [r for r in rows if r["pids"]]

    def gpu_mib(r):
        total = 0
        for a in r.get("gpu_apps") or []:
            if a["pid"] in r["pids"]:
                try:
                    total += int(a["used_MiB"])
                except ValueError:
                    return None
        return total

    def rss(r, key):
        return sum(p.get(key, 0) for p in r["proc"].values())

    doc = {
        "samples": len(rows), "with_engine": len(up), "first_t": base["t"], "last_t": rows[-1]["t"],
        "baseline_MemAvailable_kB": base.get("MemAvailable_kB"), "baseline_Cached_kB": base.get("Cached_kB"),
        "baseline_gguf_page_cache_bytes": base.get("gguf_page_cache_bytes"),
    }
    if up:
        steady = up[-1]
        doc.update(
            engine_seen_t=up[0]["t"],
            min_MemAvailable_kB=min(r["MemAvailable_kB"] for r in up),
            peak_drop_kB=base["MemAvailable_kB"] - min(r["MemAvailable_kB"] for r in up),
            steady_drop_kB=base["MemAvailable_kB"] - steady["MemAvailable_kB"],
            peak_Cached_kB=max(r["Cached_kB"] for r in up), steady_Cached_kB=steady["Cached_kB"],
            peak_rss_kB=max(rss(r, "VmRSS_kB") for r in up), steady_rss_kB=rss(steady, "VmRSS_kB"),
            steady_rss_file_kB=rss(steady, "RssFile_kB"), steady_rss_anon_kB=rss(steady, "RssAnon_kB"),
            peak_gpu_MiB=max((gpu_mib(r) or 0) for r in up), steady_gpu_MiB=gpu_mib(steady),
            gpu_used_memory_reported=any(gpu_mib(r) for r in up),
            steady_gguf_page_cache_bytes=steady.get("gguf_page_cache_bytes"),
        )
    gone = [r for r in rows if not r["pids"] and up and r["t"] > up[-1]["t"]]
    if gone:
        doc.update(after_exit_MemAvailable_kB=gone[-1]["MemAvailable_kB"],
                   released_kB=gone[-1]["MemAvailable_kB"] - up[-1]["MemAvailable_kB"],
                   after_exit_gguf_page_cache_bytes=gone[-1].get("gguf_page_cache_bytes"))
    emit(doc)
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("chat", "hangup", "streams"):
        s = sub.add_parser(name)
        s.add_argument("--dir", required=True)
        s.add_argument("--route", required=True)
        s.add_argument("--prompt", default=PROMPT)
        s.add_argument("--max-tokens", type=int, default=1024)
        s.add_argument("--timeout", type=float, default=600)
        s.add_argument("--tools", action="store_true")
        s.add_argument("--usage", action="store_true", help="stream_options.include_usage")
        if name == "chat":
            s.add_argument("--stream", action="store_true")
            s.add_argument("--expect")
        if name == "hangup":
            s.add_argument("--after-chunks", type=int, default=0, help="cut after N chunks; 0 cuts mid-prefill")
            s.add_argument("--wait-before-cut", type=float, default=0.5)
            s.add_argument("--long-prompt", type=int, default=0, help="repeat a sentence N times as the prompt")
            s.add_argument("--bound", type=float, default=10.0)
        if name == "streams":
            s.add_argument("--n", type=int, default=3)
            s.add_argument("--read-after", type=float, default=3.0)
    s = sub.add_parser("engine")
    s = sub.add_parser("load")
    s.add_argument("--dir", required=True)
    s = sub.add_parser("sample")
    s.add_argument("--gguf", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--label", default="")
    s.add_argument("--interval", type=float, default=0.5)
    s = sub.add_parser("summary")
    s.add_argument("--samples", required=True)
    args = p.parse_args()
    return {"chat": cmd_chat, "engine": cmd_engine, "load": cmd_load, "hangup": cmd_hangup, "streams": cmd_streams,
            "sample": cmd_sample, "summary": cmd_summary}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
