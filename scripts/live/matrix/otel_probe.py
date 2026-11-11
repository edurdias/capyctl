#!/usr/bin/env python3
"""Probes for the ADR 0037 live rows OT1-OT11 (scripts/live/matrix/rows/OT*.sh).

Runs on the host beside a standalone role started with its own state directory
(--dir: the inference key is read from <dir>/identity/credentials), or on the
control host against the run's server (--url, the key from CAPYCTL_API_KEY), or
against an engine directly (--url, --no-auth; OT8's SGLang trace levels). The
key is never printed or recorded, and no credential-shaped response header is
kept. Each subcommand prints JSON on standard output; `chat` and `load` also
append one record per request to --out.

  chat   one chat (POST /v1/chat/completions) or decision (POST /v1/systemone)
         request. --traceparent sends a caller trace context: sampled,
         unsampled, or a malformed one (upper, zero-trace, zero-parent,
         version-ff, short, twice: two valid headers); the record keeps the
         trace and span IDs sent, so otel_spans.py can find the request's span
         by the answer's X-CapyCTL-Request-Id and check its parent. Sentinel
         options put a marker in the user message, a system prompt, a tool
         definition and a tool call's arguments, cache_salt, X-Request-Id, the
         model name, or a field the engine refuses quoting it (OT3).
  load   N identical streamed chat requests at a fixed concurrency (or for
         --duration seconds), each with its own trace context; per request the
         router's x-capyctl-timing phases, time to first content and output
         tokens; a summary with the output tokens per second and per-phase
         percentiles (OT7, OT8).
  procs  every engine process and CapyCTL role of this user: the tracing
         options on its command line and the tracing variable names in its
         environment (OT1, OT5, OT9, OT10). Values are shown only for the
         non-secret names CapyCTL renders.
  conns  TCP sockets on the collector's ports with their owning process (ss).
  pkgs   whether an engine environment holds every package a traced engine
         imports (the launch-time check's list, capyctl-agent
         tracing_install.rs), by its files only; nothing is executed (OT10).
  stamp  follow a log file for a bounded time and record each line matching
         a pattern with the time it appeared (OT7's role log notices).

Standard library only.
"""

import argparse
import concurrent.futures
import glob
import http.client
import json
import os
import re
import secrets
import subprocess
import sys
import threading
import time
import urllib.parse

ROUTER = "http://127.0.0.1:8443"
PROMPT = "What is 17+25? Answer with only the number."
LOAD_PROMPT = ("Write a detailed, numbered list of one hundred practical tips for keeping a small vegetable "
               "garden healthy through a dry summer. Keep writing until you are stopped.")
DECISION_STATE = "I was charged twice for my order last week."
TOOL_NAME = "lookup_record"
# The router's per-request phases (crates/capyctl-router/src/timing.rs).
TIMING_PHASES = ("queue_wait_ms", "activation_wait_ms", "selection_ms", "lease_grant_ms", "pre_forward_ms",
                 "upstream_first_byte_ms", "time_to_first_byte_ms", "time_to_first_content_ms",
                 "time_to_last_chunk_ms", "total_ms")
TIMING_COMMENT = "x-capyctl-timing "
# Response headers kept in a record (never a credential).
KEPT_HEADERS = ("x-capyctl-request-id", "x-request-id", "x-capyctl-timing", "content-type")
SECRET = re.compile(r"(key|token|secret|password|credential|authorization|cookie)", re.IGNORECASE)

# ADR 0037 §7: the engines' tracing options and the reserved variable names.
TRACE_FLAGS = ("--otlp-traces-endpoint", "--collect-detailed-traces", "--enable-trace", "--otlp-service-name",
               "--trace-modules")
TRACE_ENV = re.compile(r"^(OTEL_|SGLANG_TRACE_|SGLANG_OTLP_|TRACEPARENT$|TRACESTATE$)", re.IGNORECASE)
# The variables CapyCTL renders, whose values are not secret (§7).
SHOWN_ENV = {"OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "OTEL_SERVICE_NAME", "OTEL_RESOURCE_ATTRIBUTES",
             "SGLANG_TRACE_LEVEL", "SGLANG_TRACE_ASYNC"}
ENGINE_PROC = re.compile(r"sglang[.]launch_server|sglang_entry|vllm_entry|vllm serve|EngineCore|sglang::|"
                         r"llama-server|VllmWorker")
ROLE_PROC = re.compile(r"capyctl\S* (?:.* )?start (standalone|host|server)\b")

# capyctl-agent tracing_install.rs: what a traced engine imports.
DISTRIBUTIONS = ("opentelemetry_api", "opentelemetry_sdk", "opentelemetry_proto",
                 "opentelemetry_exporter_otlp_proto_common", "opentelemetry_exporter_otlp_proto_http",
                 "opentelemetry_exporter_otlp_proto_grpc", "requests", "grpcio")
MODULES = ("opentelemetry/trace/__init__.py", "opentelemetry/trace/propagation/tracecontext.py",
           "opentelemetry/sdk/resources/__init__.py", "opentelemetry/sdk/trace/__init__.py",
           "opentelemetry/sdk/trace/export/__init__.py",
           "opentelemetry/exporter/otlp/proto/http/trace_exporter/__init__.py",
           "opentelemetry/exporter/otlp/proto/grpc/trace_exporter/__init__.py",
           "requests/__init__.py", "grpc/__init__.py")

MALFORMED = ("upper", "zero-trace", "zero-parent", "version-ff", "short", "twice")
TRACE_MODES = ("none", "sampled", "unsampled") + MALFORMED


def emit(doc):
    print(json.dumps(doc, sort_keys=True), flush=True)


def append(path, doc):
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(doc, sort_keys=True) + "\n")


def credentials(state_dir):
    creds = {}
    with open(os.path.join(state_dir, "identity", "credentials")) as f:
        for line in f:
            key, _, value = line.partition(": ")
            if value:
                creds[key.strip()] = value.strip()
    return creds


def api_key(args):
    if getattr(args, "no_auth", False):
        return None
    if args.dir:
        return credentials(args.dir)["api_key"]
    key = os.environ.get("CAPYCTL_API_KEY")
    if not key:
        sys.exit("no key: pass --dir <standalone state dir> or set CAPYCTL_API_KEY")
    return key


def ids(rng=secrets):
    """A fresh trace ID and parent span ID; each holds a hex letter, so `upper` changes it."""
    trace, span = rng.token_hex(16), rng.token_hex(8)
    return "a" + trace[1:], "b" + span[1:]


def trace_context(mode, rng=secrets):
    """The traceparent header values a mode sends, and what the record keeps of them.

    W3C Trace Context Level 1: version 00 is exactly four fields of lowercase hex
    with a non-zero trace ID and parent ID. sampled and unsampled are valid (flags
    01 and 00); every other mode but none is invalid, so ADR 0037 §2 ignores it
    and starts a new trace.
    """
    if mode == "none":
        return [], {"mode": "none", "valid": False}
    trace, span = ids(rng)
    rec = {"mode": mode, "trace_id": trace, "span_id": span, "valid": mode in ("sampled", "unsampled"),
           "sampled": mode == "sampled"}
    if mode == "sampled":
        return [f"00-{trace}-{span}-01"], rec
    if mode == "unsampled":
        return [f"00-{trace}-{span}-00"], rec
    if mode == "upper":
        return [f"00-{trace.upper()}-{span.upper()}-01"], rec
    if mode == "zero-trace":
        rec["trace_id"] = "0" * 32
        return [f"00-{'0' * 32}-{span}-01"], rec
    if mode == "zero-parent":
        rec["span_id"] = "0" * 16
        return [f"00-{trace}-{'0' * 16}-01"], rec
    if mode == "version-ff":
        return [f"ff-{trace}-{span}-01"], rec
    if mode == "short":
        return [f"00-{trace}-{span}"], rec
    if mode == "twice":
        trace2, span2 = ids(rng)
        rec["second"] = {"trace_id": trace2, "span_id": span2}
        return [f"00-{trace}-{span}-01", f"00-{trace2}-{span2}-01"], rec
    raise ValueError(f"unknown trace mode {mode}")


def chat_body(args, stream=None, prompt=None):
    """The chat request; sentinel options place their markers (OT3)."""
    stream = args.stream if stream is None else stream
    user = prompt or args.prompt
    if getattr(args, "sentinel_message", None):
        user = f"Reference {args.sentinel_message}. {user}"
    messages = []
    if getattr(args, "sentinel_system", None):
        messages.append({"role": "system", "content": f"You are a helpful assistant for case {args.sentinel_system}."})
    messages.append({"role": "user", "content": user})
    body = {"model": args.model or args.route, "messages": messages, "max_tokens": args.max_tokens,
            "temperature": 0, "stream": stream}
    if stream:
        body["stream_options"] = {"include_usage": True}
    tool_marker = getattr(args, "sentinel_tool", None) or getattr(args, "engine_error", None)
    if tool_marker:
        body["tools"] = [{"type": "function", "function": {
            "name": TOOL_NAME, "description": f"Look up a record such as {tool_marker}.",
            "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}}]
    if getattr(args, "sentinel_tool", None):
        marker = args.sentinel_tool
        messages.append({"role": "assistant", "content": "", "tool_calls": [{
            "id": "call_ot3", "type": "function",
            "function": {"name": TOOL_NAME, "arguments": json.dumps({"id": marker})}}]})
        messages.append({"role": "tool", "tool_call_id": "call_ot3", "content": f"record {marker}: no balance"})
        messages.append({"role": "user", "content": "Thanks. Now answer: what is 17+25?"})
    if getattr(args, "cache_salt", None):
        body["cache_salt"] = args.cache_salt
    if getattr(args, "engine_error", None):
        marker = args.engine_error
        if args.engine_error_kind == "tool_choice":
            # A forced tool the request does not define: the engine refuses it and
            # names it in its error.
            body["tool_choice"] = {"type": "function", "function": {"name": marker}}
        else:
            # A JSON schema with a type no validator knows.
            body["response_format"] = {"type": "json_schema",
                                       "json_schema": {"name": "ot3", "schema": {"type": marker}}}
    return body


def decision_body(args):
    marker = getattr(args, "sentinel_message", None)
    state = f"{DECISION_STATE} Order {marker}." if marker else DECISION_STATE
    instructions = f"Is case {marker} about billing?" if marker else "Is this about billing?"
    return {"model": args.model or args.route, "state": state,
            "questions": {"billing": {"type": "noul", "instructions": instructions}}}


def open_post(url, path, body, headers, timeout):
    u = urllib.parse.urlsplit(url)
    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)
    data = json.dumps(body).encode()
    conn.putrequest("POST", path)
    for name, value in headers:
        conn.putheader(name, value)
    conn.putheader("Content-Type", "application/json")
    conn.putheader("Content-Length", str(len(data)))
    conn.endheaders(data)
    return conn, conn.getresponse()


def request_headers(key, traceparents, tracestate=None, x_request_id=None):
    headers = []
    if key:
        headers.append(("Authorization", f"Bearer {key}"))
    for value in traceparents:
        headers.append(("traceparent", value))
    if tracestate:
        headers.append(("tracestate", tracestate))
    if x_request_id:
        headers.append(("X-Request-Id", x_request_id))
    return headers


def parse_sse(lines):
    """Content, usage, finish reason, chunk count and the router's timing comment of an SSE stream."""
    out = {"chunks": 0, "content": "", "usage": None, "finish_reason": None, "timing": None, "done": False,
           "first_content_index": None}
    for raw in lines:
        line = raw.rstrip("\r\n")
        if line.startswith(":"):
            comment = line[1:].strip()
            if comment.startswith(TIMING_COMMENT):
                try:
                    out["timing"] = json.loads(comment[len(TIMING_COMMENT):])
                except ValueError:
                    out["timing"] = {"malformed": comment[:200]}
            continue
        if not line.startswith("data: "):
            continue
        payload = line[6:]
        if payload == "[DONE]":
            out["done"] = True
            continue
        try:
            chunk = json.loads(payload)
        except ValueError:
            continue
        out["chunks"] += 1
        if chunk.get("usage"):
            out["usage"] = chunk["usage"]
        for choice in chunk.get("choices") or []:
            delta = choice.get("delta") or {}
            text = (delta.get("content") or "") + (delta.get("reasoning_content") or "")
            if text and out["first_content_index"] is None:
                out["first_content_index"] = out["chunks"]
            out["content"] += delta.get("content") or ""
            if choice.get("finish_reason"):
                out["finish_reason"] = choice["finish_reason"]
    return out


def header_timing(resp):
    value = resp.getheader("x-capyctl-timing")
    if not value:
        return None
    try:
        return json.loads(value)
    except ValueError:
        return {"malformed": value[:200]}


def send(args, key, body, path, trace_mode, tracestate=None, x_request_id=None, stream=False):
    """One request; the record of what was sent and what came back."""
    traceparents, caller = trace_context(trace_mode)
    headers = request_headers(key, traceparents, tracestate, x_request_id)
    rec = {"label": args.label, "route": args.route, "path": path, "stream": stream, "caller": caller,
           "tracestate_sent": bool(tracestate), "x_request_id_sent": bool(x_request_id), "sent_at": time.time()}
    t0 = time.monotonic()
    try:
        conn, resp = open_post(args.url, path, body, headers, args.timeout)
    except OSError as e:
        rec.update(status=None, ok=False, error=f"{type(e).__name__}: {e}"[:300])
        return rec
    rec["status"] = resp.status
    rec["request_id"] = resp.getheader("x-capyctl-request-id")
    rec["x_request_id_echo"] = resp.getheader("x-request-id") is not None
    rec["response_header_names"] = sorted({h.lower() for h, _ in resp.getheaders() if not SECRET.search(h)})
    rec["timing"] = header_timing(resp)
    first_ms = None
    if resp.status != 200:
        text = resp.read().decode(errors="replace")
        rec["error_body"] = text[:600]
        try:
            rec["error_code"] = json.loads(text).get("code")
        except (ValueError, AttributeError):
            rec["error_code"] = None
    elif stream:
        lines = []
        for raw in resp:
            line = raw.decode(errors="replace")
            lines.append(line)
            if first_ms is None and parse_sse([line])["first_content_index"] is not None:
                first_ms = round((time.monotonic() - t0) * 1000, 3)
        parsed = parse_sse(lines)
        rec.update(chunks=parsed["chunks"], done=parsed["done"], usage=parsed["usage"],
                   finish_reason=parsed["finish_reason"], content_tail=parsed["content"][-160:])
        if parsed["timing"]:
            rec["timing"] = parsed["timing"]
    else:
        text = resp.read().decode(errors="replace")
        try:
            doc = json.loads(text)
        except ValueError:
            doc = {}
            rec["body_head"] = text[:300]
        if path.endswith("/systemone"):
            rec["answers"] = sorted((doc.get("answers") or {}).keys())
            rec["usage"] = doc.get("usage")
        else:
            choice = (doc.get("choices") or [{}])[0]
            message = choice.get("message") or {}
            rec.update(usage=doc.get("usage"), finish_reason=choice.get("finish_reason"),
                       content_tail=(message.get("content") or "")[-160:],
                       tool_calls=[(c.get("function") or {}).get("name") for c in message.get("tool_calls") or []])
    conn.close()
    rec["ms"] = round((time.monotonic() - t0) * 1000, 3)
    rec["first_content_ms"] = first_ms
    return rec


def cmd_chat(args):
    key = api_key(args)
    if args.api == "decisions":
        body, path, stream = decision_body(args), "/v1/systemone", False
    else:
        body, path, stream = chat_body(args), "/v1/chat/completions", args.stream
    rec = send(args, key, body, path, args.traceparent, args.tracestate, args.x_request_id, stream)
    markers = [m for m in (args.sentinel_message, args.sentinel_system, args.sentinel_tool, args.cache_salt,
                           args.x_request_id, args.engine_error, args.model) if m]
    if markers:
        rec["sentinels"] = markers
        # Evidence that the request carried what OT3 claims: the engine's error
        # quotes its input (else the sentinel never reached anything to quote).
        rec["error_quotes_sentinel"] = any(m in (rec.get("error_body") or "") for m in markers)
    if args.expect_status is not None:
        rec["ok"] = rec.get("status") == args.expect_status
    elif args.expect is not None:
        rec["ok"] = rec.get("status") == 200 and args.expect in (rec.get("content_tail") or "")
    else:
        rec["ok"] = rec.get("status") == 200
    append(args.out, rec)
    emit(rec)
    return 0 if rec["ok"] else 1


def percentiles(values):
    values = sorted(v for v in values if isinstance(v, (int, float)))
    if not values:
        return None
    pick = lambda q: values[min(len(values) - 1, int(round(q * (len(values) - 1))))]  # noqa: E731
    return {"n": len(values), "mean": round(sum(values) / len(values), 3), "p50": pick(0.5), "p90": pick(0.9),
            "p99": pick(0.99), "min": values[0], "max": values[-1]}


def load_summary(label, records, wall_s, trace_mode, concurrency):
    ok = [r for r in records if r.get("status") == 200 and r.get("done", True)]
    statuses = {}
    for r in records:
        statuses[str(r.get("status"))] = statuses.get(str(r.get("status")), 0) + 1
    tokens = sum(((r.get("usage") or {}).get("completion_tokens") or 0) for r in ok)
    timing = {}
    for phase in TIMING_PHASES:
        stats = percentiles([(r.get("timing") or {}).get(phase) for r in ok])
        if stats:
            timing[phase] = stats
    return {"label": label, "traceparent": trace_mode, "concurrency": concurrency, "n": len(records),
            "ok": len(ok), "status_counts": statuses, "wall_s": round(wall_s, 3), "output_tokens": tokens,
            "output_tokens_per_s": round(tokens / wall_s, 3) if wall_s > 0 else None,
            "latency_ms": percentiles([r.get("ms") for r in ok]),
            "first_content_ms": percentiles([r.get("first_content_ms") for r in ok]),
            "timing": timing}


def cmd_load(args):
    key = api_key(args)
    lock = threading.Lock()
    records = []
    deadline = time.monotonic() + args.duration if args.duration else None
    counter = {"next": 0}

    def take():
        with lock:
            if deadline is None:
                if counter["next"] >= args.n:
                    return None
            elif time.monotonic() >= deadline:
                return None
            counter["next"] += 1
            return counter["next"]

    def worker():
        while True:
            index = take()
            if index is None:
                return
            body = chat_body(args, stream=True, prompt=args.prompt or LOAD_PROMPT)
            rec = send(args, key, body, "/v1/chat/completions", args.traceparent, stream=True)
            rec["index"] = index
            with lock:
                records.append(rec)
                append(args.out, rec)

    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
        for future in [pool.submit(worker) for _ in range(args.concurrency)]:
            future.result()
    summary = load_summary(args.label, records, time.monotonic() - t0, args.traceparent, args.concurrency)
    if args.summary:
        with open(args.summary, "w") as handle:
            json.dump(summary, handle, indent=1, sort_keys=True)
    emit(summary)
    return 0 if summary["n"] and summary["ok"] == summary["n"] else 1


def tracing_args(argv):
    """The tracing options on a command line, in both spellings, with their values."""
    found = []
    for i, arg in enumerate(argv):
        name, eq, value = arg.partition("=")
        if name in TRACE_FLAGS:
            if not eq and i + 1 < len(argv) and not argv[i + 1].startswith("--"):
                value = argv[i + 1]
            found.append([name, value])
    return found


def tracing_env(environ):
    """Tracing variable names in an environment; values only for the names CapyCTL renders."""
    out = {}
    for name, value in environ.items():
        if TRACE_ENV.match(name):
            out[name] = value if name.upper() in SHOWN_ENV else "<set>"
    return out


def read_environ(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            raw = f.read()
    except OSError as e:
        return None, type(e).__name__
    env = {}
    for item in raw.split(b"\0"):
        name, eq, value = item.partition(b"=")
        if eq:
            env[name.decode(errors="replace")] = value.decode(errors="replace")
    return env, None


# Launchers whose command line names a role or an engine they run: the harness's
# tmux server, role_exec.sh, `env`, `timeout`, `nohup`, shells, ssh, pgrep and ps.
WRAPPERS = {"tmux", "bash", "sh", "env", "timeout", "nohup", "ssh", "pgrep", "ps"}


def classify(cmdline):
    first = os.path.basename(cmdline.split(" ", 1)[0]) if cmdline else ""
    if "otel_probe.py" in cmdline or first in WRAPPERS or first.startswith("tmux"):
        return None
    if ENGINE_PROC.search(cmdline):
        return "engine"
    if ROLE_PROC.search(cmdline):
        return "capyctl"
    return None


def cmd_procs(args):
    uid, me = os.getuid(), os.getpid()
    procs = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or int(entry) in (me, os.getppid()):
            continue
        try:
            if os.stat(f"/proc/{entry}").st_uid != uid:
                continue
            with open(f"/proc/{entry}/cmdline", "rb") as f:
                argv = [a.decode(errors="replace") for a in f.read().split(b"\0") if a]
        except OSError:
            continue
        kind = classify(" ".join(argv))
        if not argv or kind is None:
            continue
        env, err = read_environ(entry)
        procs.append({"pid": int(entry), "kind": kind, "argv0": os.path.basename(argv[0]),
                      "tracing_args": tracing_args(argv),
                      "tracing_env": tracing_env(env) if env is not None else None, "environ_error": err})
    procs.sort(key=lambda p: p["pid"])
    emit({"processes": procs,
          "with_tracing_args": sum(1 for p in procs if p["tracing_args"]),
          "with_tracing_env": sum(1 for p in procs if p["tracing_env"])})
    return 0


def parse_ss(text, ports):
    """Sockets of `ss -Htanp` whose local or peer port is one of ports."""
    out = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 5 or fields[0] == "State":
            continue
        state, local, peer = fields[0], fields[3], fields[4]
        proc = " ".join(fields[5:])
        lport, pport = local.rsplit(":", 1)[-1], peer.rsplit(":", 1)[-1]
        if lport in ports or pport in ports:
            names = re.findall(r'\("([^"]+)",pid=(\d+)', proc)
            out.append({"state": state, "local": local, "peer": peer,
                        "process": [{"name": n, "pid": int(p)} for n, p in names],
                        "to_collector": pport in ports and state != "LISTEN"})
    return out


def cmd_conns(args):
    ports = set(args.ports.split(","))
    text = subprocess.run(["ss", "-Htanp"], capture_output=True, text=True).stdout
    socks = parse_ss(text, ports)
    emit({"ports": sorted(ports), "sockets": socks, "to_collector": sum(1 for s in socks if s["to_collector"])})
    return 0


def tracing_packages(venv):
    """What `venv` lacks of the packages a traced engine imports (files only)."""
    sites = sorted(glob.glob(os.path.join(venv, "lib", "python3*", "site-packages")))
    if not sites:
        return {"venv": venv, "site_packages": None, "missing": ["site-packages"], "complete": False}
    site = sites[-1]
    missing = []
    for name in DISTRIBUTIONS:
        if not glob.glob(os.path.join(site, f"{name}-*.dist-info")) and \
                not glob.glob(os.path.join(site, f"{name.lower()}-*.dist-info")):
            missing.append(f"{name}-*.dist-info")
    missing += [m for m in MODULES if not os.path.isfile(os.path.join(site, m))]
    return {"venv": venv, "site_packages": site, "missing": missing, "complete": not missing}


def cmd_pkgs(args):
    doc = tracing_packages(args.venv)
    emit(doc)
    return 0


def gaps(times):
    return [round(b - a, 3) for a, b in zip(times, times[1:])]


def cmd_stamp(args):
    pattern = re.compile(args.match)
    deadline = time.monotonic() + args.seconds
    seen = []
    with open(args.file, errors="replace") as handle:
        handle.seek(0, os.SEEK_END)
        while time.monotonic() < deadline:
            line = handle.readline()
            if not line:
                time.sleep(0.2)
                continue
            if pattern.search(line):
                rec = {"t": time.time(), "line": line.rstrip("\n")[:400]}
                seen.append(rec)
                append(args.out, rec)
    times = [r["t"] for r in seen]
    emit({"file": args.file, "match": args.match, "seconds": args.seconds, "lines": len(seen),
          "gaps_s": gaps(times), "min_gap_s": min(gaps(times)) if len(times) > 1 else None})
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("chat", "load"):
        s = sub.add_parser(name)
        s.add_argument("--dir", help="the standalone role's state directory (its credentials)")
        s.add_argument("--url", default=os.environ.get("CAPYCTL_INFER_URL", ROUTER))
        s.add_argument("--no-auth", action="store_true", help="an engine's own port: no Authorization header")
        s.add_argument("--route", required=True)
        s.add_argument("--model", help="the model field when it differs from --route (OT3: an unknown sentinel)")
        s.add_argument("--label", default="")
        s.add_argument("--out", help="append one JSON record per request")
        s.add_argument("--traceparent", choices=TRACE_MODES, default="none")
        s.add_argument("--max-tokens", type=int, default=1024 if name == "chat" else 256)
        s.add_argument("--timeout", type=float, default=1200)
        if name == "chat":
            s.add_argument("--api", choices=("chat", "decisions"), default="chat")
            s.add_argument("--stream", action="store_true")
            s.add_argument("--prompt", default=PROMPT)
            s.add_argument("--tracestate")
            s.add_argument("--x-request-id")
            s.add_argument("--sentinel-message")
            s.add_argument("--sentinel-system")
            s.add_argument("--sentinel-tool")
            s.add_argument("--cache-salt")
            s.add_argument("--engine-error", help="a marker the engine refuses and quotes")
            s.add_argument("--engine-error-kind", choices=("tool_choice", "response_format"), default="tool_choice")
            s.add_argument("--expect", help="text the answer must contain")
            s.add_argument("--expect-status", type=int)
        else:
            s.add_argument("--prompt")
            s.add_argument("--n", type=int, default=200)
            s.add_argument("--duration", type=float, help="send for this many seconds instead of --n requests")
            s.add_argument("--concurrency", type=int, default=8)
            s.add_argument("--summary")
    sub.add_parser("procs")
    s = sub.add_parser("conns")
    s.add_argument("--ports", default="4317,4318")
    s = sub.add_parser("pkgs")
    s.add_argument("--venv", required=True)
    s = sub.add_parser("stamp")
    s.add_argument("--file", required=True)
    s.add_argument("--match", required=True)
    s.add_argument("--seconds", type=float, required=True)
    s.add_argument("--out")
    args = p.parse_args()
    if args.cmd in ("chat", "load") and not args.label:
        args.label = args.route
    return {"chat": cmd_chat, "load": cmd_load, "procs": cmd_procs, "conns": cmd_conns, "pkgs": cmd_pkgs,
            "stamp": cmd_stamp}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
