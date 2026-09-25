#!/usr/bin/env python3
"""Performance benchmark through the shipped router (matrix row M80; SPEC section 17, T40).

usage:
  bench.py run      --route R --prompt-tokens L --concurrency C [--repeats N] [--warmup W]
                    [--max-tokens 256] [--context-length 32768] [--seed 8080]
                    [--calibration WARMUP.json] [--warmup-only] [--no-ignore-eos]
                    --out CELL.json --records RECORDS.jsonl
  bench.py once     --route R --label NAME [--prompt-tokens 128] [--max-tokens 32]
                    --out LIFECYCLE.jsonl          (one request, timed to its first token)
  bench.py tcprtt   --target NAME=HOST:PORT [...] [--samples 20] --out NETFLOOR.json
  bench.py promdelta --before A.prom --after B.prom  (engine /metrics histogram deltas)
  bench.py latencydelta --before A.json --after B.json [--cell CELL.json]
                    (mllm latency API window: router, ingress and engine series)
  bench.py report   --evid DIR [--fixture NAME]    (writes DIR/bench.json and DIR/summary.md)

Every request is one streaming OpenAI chat completion to the server's loopback
router with `stream_options.include_usage`, temperature 0 and, unless the engine
refuses it, `ignore_eos`. For each request the record keeps the send time, the
arrival time of every SSE chunk that carries generated text, the first and the
last such chunk, and the token counts from the final `usage` chunk. When the
stream carries no usage, the counts are estimated without a tokenizer and the
record says so (`usage_source`).

Metric definitions (all times from the client's monotonic clock):
  TTFT           t_first_token - t_send, where t_send is taken just before the
                 connection opens and t_first_token is the arrival of the first
                 chunk whose delta has non-empty `content` (or `reasoning_content`).
  TTLT           t_last_token - t_send (last chunk with generated text).
  prefill tok/s  prompt_tokens / TTFT. At concurrency > 1 TTFT includes queueing,
                 so this is an effective rate, not the kernel's prefill rate.
  decode tok/s   (completion_tokens - 1) / (TTLT - TTFT), per request.
  TPOT           (TTLT - TTFT) / (completion_tokens - 1).
  ITL            gaps between successive text chunks; pooled over a cell and
                 reported as p50/p95/p99. A chunk may carry more than one token;
                 `tokens_per_chunk` in the cell shows whether it did.
  throughput     cell totals over the measured phase's wall time (first send to
                 last finish): output tok/s, total tok/s and requests/s.

Prompts are synthetic and deterministic: seeded word sequences from a fixed
vocabulary, a unique per-request header (so no two requests share a prefix
beyond the chat template) and an instruction to keep writing. The target
length is capped to context_length - max_tokens - 128. The warmup requests
calibrate words per token from the reported usage; the measured phase reuses
that calibration. Warmup requests are never part of the measured statistics.

mllm-level timings (owner decision 2026-09-23): the server's latency view
(`GET /management/v1/metrics/latency`, also embedded as `latency` in
`mllm status deployment --output json`) holds bucketed distributions per
instance: router phases (tier router), host ingress times (tier ingress) and the
engine's own histograms (tier engine, `source: engine`) for both engines. The row
saves that view before and after each cell through the CLI; `latencydelta`
subtracts the two bucket by bucket, so each cell gets its own window, and
`report` derives the path overhead from it. With the server's
`observability.timing_header` on, each streamed response also ends with an SSE
comment `: x-mllm-timing {...}` holding that request's router phases; `run`
keeps it per record (`mllm_timing`) and summarizes it per cell.

Secrets: the API key comes from MLLM_API_KEY only (lib.sh load_api_key) and is
never printed or recorded; credential-shaped response headers are dropped.
Standard library only.
"""

import argparse
import glob
import http.client
import json
import math
import os
import random
import re
import socket
import sys
import threading
import time
import urllib.parse
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import matrixhttp  # noqa: E402  (api_key, DEFAULT_BASE; unchanged)

VOCAB = (
    "the of and to in is was for on that with as by at from his her they this have had not are but "
    "one all were when we there can an which their said if do will each about how up out them then "
    "she many some so these would other into has more two time could no make than first been its who "
    "now people my made over did down only way find use may water long little very after words called "
    "just where most know get through back much before go good new write our used me man too any day "
    "same right look think also around another came come work three word must because does part even "
    "place well such here take why help put different away again off went old number great tell men "
    "say small every found still between name should home big give air line set own under read last "
    "never us left end along while might next sound below saw something thought both few those always "
    "show large often together asked house world going want school important until form food keep "
    "children feet land side without boy once animal life enough took four head above kind began "
    "almost live page got earth need far hand high year mother light country father let night picture "
    "being study second soon story since white ever paper hard near sentence better best across during "
    "today however sure knew try told young sun thing whole hear example heard several change answer "
    "room sea against top turned learn point city play toward five himself usually money seen car "
    "morning river table field road map rain cold notice voice energy hunt wheel full force blue object"
).split()
INSTRUCTION = ("\n\nIgnore the notes above. Write a numbered list that counts upward from 1, one number "
               "per line, each followed by a short phrase about the sea. Keep going and do not stop.")
CONTEXT_MARGIN = 128
TIMING_COMMENT = b"x-mllm-timing "
_DROP = ("authorization", "cookie", "set-cookie", "x-api-key")


# ---------------------------------------------------------------------------- prompts

def make_prompt(seed, tag, index, target_tokens, words_per_token=1.0):
    """Deterministic synthetic prompt of about target_tokens tokens."""
    rng = random.Random(f"{seed}:{tag}:{index}")
    header = f"Benchmark request {tag}-{index}-{rng.randrange(10**9):09d}. Notes follow.\n"
    budget = max(8, int(round(target_tokens * words_per_token)) - 60)
    words = []
    for n in range(budget):
        word = rng.choice(VOCAB)
        words.append(word + ("." if n % 14 == 13 else ""))
    return header + " ".join(words) + INSTRUCTION, len(words)


def cap_target(target, context_length, max_tokens):
    limit = context_length - max_tokens - CONTEXT_MARGIN
    return (min(target, limit), target if target > limit else None)


# ---------------------------------------------------------------------------- one request

def stream_chat(route, prompt, *, max_tokens, ignore_eos, base, timeout, marker=None):
    """One streaming chat completion with per-chunk arrival times. Never raises on HTTP errors."""
    marker = marker or uuid.uuid4().hex
    body = {"model": route, "messages": [{"role": "user", "content": prompt}], "max_tokens": max_tokens,
            "temperature": 0.0, "stream": True, "stream_options": {"include_usage": True}}
    if ignore_eos:
        body["ignore_eos"] = True
    parsed = urllib.parse.urlparse(base)
    rec = {"marker": marker, "route": route, "max_tokens": max_tokens, "ignore_eos": ignore_eos,
           "prompt_chars": len(prompt)}
    t_send = time.monotonic()
    rec["t_send_unix_ms"] = int(time.time() * 1000)
    chunks = []          # seconds after t_send of each chunk carrying generated text
    text_kinds = set()
    usage = None
    finish = None
    events = malformed = 0
    done = False
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout)
    try:
        conn.request("POST", "/v1/chat/completions", body=json.dumps(body).encode(),
                     headers={"Authorization": "Bearer " + matrixhttp.api_key(),
                              "Content-Type": "application/json", "x-request-id": marker})
        resp = conn.getresponse()
        rec["status"] = resp.status
        rec["headers"] = {k.lower(): v for k, v in resp.getheaders() if k.lower() not in _DROP}
        if resp.status != 200:
            raw = resp.read()
            rec["error_text"] = raw[:400].decode(errors="replace")
        else:
            buffer = b""
            while True:
                data = resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536)
                arrived = time.monotonic() - t_send
                if not data:
                    break
                buffer = (buffer + data).replace(b"\r\n", b"\n")
                while b"\n\n" in buffer:
                    event, buffer = buffer.split(b"\n\n", 1)
                    event = event.strip(b"\n")
                    if not event:
                        continue
                    if event.startswith(b":"):
                        # SSE comment (ignored by SSE clients). The router's
                        # per-request timings arrive as one when enabled.
                        comment = event[1:].strip()
                        if comment.startswith(TIMING_COMMENT):
                            try:
                                rec["mllm_timing"] = json.loads(comment[len(TIMING_COMMENT):])
                            except ValueError:
                                malformed += 1
                        continue
                    if not event.startswith(b"data: "):
                        malformed += 1
                        continue
                    payload = event[6:]
                    events += 1
                    if payload == b"[DONE]":
                        done = True
                        continue
                    try:
                        obj = json.loads(payload)
                    except ValueError:
                        malformed += 1
                        continue
                    if obj.get("usage"):
                        usage = obj["usage"]
                    got_text = False
                    for choice in obj.get("choices") or []:
                        delta = choice.get("delta") or {}
                        for kind in ("content", "reasoning_content"):
                            if delta.get(kind):
                                got_text = True
                                text_kinds.add(kind)
                        if choice.get("finish_reason"):
                            finish = choice["finish_reason"]
                    if got_text:
                        chunks.append(arrived)
            if buffer.strip():
                malformed += 1
    except (OSError, http.client.HTTPException) as error:
        rec.setdefault("status", None)
        rec["transport_error"] = f"{type(error).__name__}: {error}"
    finally:
        conn.close()
    rec["elapsed_s"] = round(time.monotonic() - t_send, 6)
    rec.update({"chunk_offsets_s": [round(c, 6) for c in chunks], "text_kinds": sorted(text_kinds),
                "usage": usage, "finish_reason": finish, "sse_events": events, "sse_malformed": malformed,
                "sse_done": done})
    return rec


def derive(rec, estimated_prompt_tokens):
    """Per-request metrics from the raw record (pure; tested against a fake server)."""
    chunks = rec.get("chunk_offsets_s") or []
    usage = rec.get("usage") or {}
    ok = rec.get("status") == 200 and bool(chunks) and rec.get("sse_done") and not rec.get("sse_malformed")
    m = {"ok": bool(ok)}
    if not chunks:
        return m
    ttft, ttlt = chunks[0], chunks[-1]
    if usage.get("prompt_tokens") is not None and usage.get("completion_tokens") is not None:
        prompt_tokens, completion_tokens, source = usage["prompt_tokens"], usage["completion_tokens"], "usage"
    else:
        # Tokenizer-free estimate: one token per text chunk (engines stream one
        # token per chunk unless they batch) and the calibrated prompt estimate.
        prompt_tokens, completion_tokens, source = estimated_prompt_tokens, len(chunks), "estimate"
    gaps = [b - a for a, b in zip(chunks, chunks[1:])]
    m.update({
        "ttft_s": round(ttft, 6), "ttlt_s": round(ttlt, 6),
        "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "usage_source": source,
        "chunks": len(chunks),
        "prefill_tps": round(prompt_tokens / ttft, 3) if ttft > 0 and prompt_tokens else None,
        "decode_tps": (round((completion_tokens - 1) / (ttlt - ttft), 3)
                       if completion_tokens and completion_tokens > 1 and ttlt > ttft else None),
        "tpot_s": (round((ttlt - ttft) / (completion_tokens - 1), 6)
                   if completion_tokens and completion_tokens > 1 else None),
        "itl_s": [round(g, 6) for g in gaps],
        # Stream end ([DONE] arrival). With ignore_eos, tokens after the model's
        # own end of sequence often detokenize to no text, so the last text
        # chunk comes early and TTLT/decode from it overstate speed (found live
        # 2026-09-25, MiniCPM5-2B); these use the whole stream.
        "e2e_s": rec.get("elapsed_s"),
        "decode_e2e_tps": (round((completion_tokens - 1) / (rec["elapsed_s"] - ttft), 3)
                           if completion_tokens and completion_tokens > 1 and rec.get("elapsed_s")
                           and rec["elapsed_s"] > ttft else None),
        "short_completion": bool(completion_tokens is not None and completion_tokens < rec["max_tokens"]),
    })
    return m


# ---------------------------------------------------------------------------- statistics

def pct(values, q):
    """Linear-interpolated percentile (q in 0..100); None for no values."""
    vals = sorted(v for v in values if v is not None)
    if not vals:
        return None
    if len(vals) == 1:
        return vals[0]
    pos = (len(vals) - 1) * q / 100.0
    lo = math.floor(pos)
    hi = min(lo + 1, len(vals) - 1)
    return vals[lo] + (vals[hi] - vals[lo]) * (pos - lo)


def dist(values, qs=(50, 90, 95, 99)):
    vals = [v for v in values if v is not None]
    out = {"n": len(vals), "mean": round(sum(vals) / len(vals), 6) if vals else None}
    for q in qs:
        v = pct(vals, q)
        out[f"p{q}"] = round(v, 6) if v is not None else None
    return out


# ---------------------------------------------------------------------------- run (one cell)

def run_phase(args, route, tag, count_per_worker, words_per_token, target, index_base, ignore_eos, records_path):
    lock = threading.Lock()
    results = []
    barrier = threading.Barrier(args.concurrency)

    def worker(w):
        barrier.wait()
        for r in range(count_per_worker):
            index = index_base + w * count_per_worker + r
            prompt, words = make_prompt(args.seed, tag, index, target, words_per_token)
            rec = stream_chat(route, prompt, max_tokens=args.max_tokens, ignore_eos=ignore_eos[0],
                              base=args.base, timeout=args.timeout)
            rec.update({"cell": tag, "index": index, "worker": w, "words": words,
                        "target_prompt_tokens": target, "phase": "warmup" if index_base == 0 else "measure"})
            with lock:
                results.append(rec)
                if records_path:
                    with open(records_path, "a") as handle:
                        handle.write(json.dumps(rec, sort_keys=True) + "\n")

    threads = [threading.Thread(target=worker, args=(w,), daemon=True) for w in range(args.concurrency)]
    started = time.monotonic()
    started_unix = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return results, started, started_unix, time.monotonic()


def ignore_eos_refused(rec):
    return rec.get("status") in (400, 422) and "ignore_eos" in (rec.get("error_text") or "")


def cmd_run(args):
    if not 1 <= args.concurrency <= 256:
        sys.exit("--concurrency must be 1..256")
    target, capped_from = cap_target(args.prompt_tokens, args.context_length, args.max_tokens)
    tag = f"L{args.prompt_tokens}-C{args.concurrency}"
    words_per_token = 1.0
    ignore = [not args.no_ignore_eos]
    notes = []
    calib = None
    if args.calibration:
        with open(args.calibration) as handle:
            calib = json.load(handle)
        calib = calib.get("calibration", calib)  # a --warmup-only cell file nests it
        words_per_token = calib.get("words_per_token", 1.0)
        ignore = [calib.get("ignore_eos", ignore[0])]

    cell = {"cell": tag, "route": args.route, "prompt_tokens_requested": args.prompt_tokens,
            "prompt_tokens_target": target, "capped_from": capped_from, "context_length": args.context_length,
            "max_tokens": args.max_tokens, "concurrency": args.concurrency, "seed": args.seed,
            "temperature": 0.0}

    if args.warmup_only or not args.calibration:
        # Warmup: excluded from statistics; calibrates words per token and
        # learns whether the engine accepts ignore_eos.
        warm, _, _, _ = run_phase(args, args.route, tag, max(1, args.warmup), 1.0, target, 0, ignore, args.records)
        if ignore[0] and warm and all(ignore_eos_refused(r) for r in warm):
            notes.append("ignore_eos refused by the engine; the long-generation prompt is used alone")
            ignore = [False]
            warm, _, _, _ = run_phase(args, args.route, tag, max(1, args.warmup), 1.0, target, 0, ignore, args.records)
        ratios = [r["target_prompt_tokens"] / r["usage"]["prompt_tokens"] for r in warm
                  if r.get("status") == 200 and (r.get("usage") or {}).get("prompt_tokens")]
        if ratios:
            words_per_token = round(sum(ratios) / len(ratios), 4)
        else:
            notes.append("no usage in warmup; prompt length uncalibrated (1 word per token assumed)")
        calib = {"words_per_token": words_per_token, "ignore_eos": ignore[0],
                 "warmup_status": sorted({str(r.get("status")) for r in warm}), "warmup_count": len(warm)}
        if args.warmup_only:
            cell.update({"calibration": calib, "notes": notes, "warmup_only": True})
            write_json(args.out, cell)
            print(json.dumps({"cell": tag, "calibration": calib, "notes": notes}))
            return 0 if all(r.get("status") == 200 for r in warm) else 1

    measured, t0, t0_unix, t1 = run_phase(args, args.route, tag, args.repeats, words_per_token, target, 10**6,
                                          ignore, args.records)
    cell.update({"calibration": calib, "notes": notes})
    cell.update(summarize_cell(measured, words_per_token, t0, t1))
    timing = mllm_timing_summary(measured)
    if timing:
        cell["mllm_timing"] = timing
    cell["measure_started_unix"] = round(t0_unix, 3)
    write_json(args.out, cell)
    print(json.dumps({k: cell[k] for k in ("cell", "requests", "ok", "ttft_s", "decode_tps", "throughput")},
                     sort_keys=True))
    return 0 if cell["ok"] == cell["requests"] else 1


def summarize_cell(measured, words_per_token, t0=None, t1=None):
    per = []
    for rec in measured:
        est = int(round(rec.get("words", 0) / words_per_token)) + 60 if words_per_token else None
        per.append((rec, derive(rec, est)))
    good = [m for _, m in per if m["ok"]]
    itl = [g for m in good for g in m.get("itl_s", [])]
    # Wall time of the measured phase from the records: first send to last finish.
    sends = [r["t_send_unix_ms"] / 1000.0 for r, _ in per]
    ends = [r["t_send_unix_ms"] / 1000.0 + r["elapsed_s"] for r, _ in per]
    wall = (max(ends) - min(sends)) if per else None
    if t0 is not None and t1 is not None:
        wall = t1 - t0
    out_tokens = sum(m["completion_tokens"] for m in good)
    in_tokens = sum(m["prompt_tokens"] or 0 for m in good)
    chunks = sum(m["chunks"] for m in good)
    return {
        "requests": len(per), "ok": len(good),
        "failed": [{"index": r.get("index"), "status": r.get("status"),
                    "error": (r.get("error_text") or r.get("transport_error") or "")[:200]}
                   for r, m in per if not m["ok"]],
        "usage_source": sorted({m["usage_source"] for m in good}),
        "short_completions": sum(1 for m in good if m.get("short_completion")),
        "prompt_tokens": dist([m["prompt_tokens"] for m in good], (50,)),
        "completion_tokens": dist([m["completion_tokens"] for m in good], (50,)),
        "ttft_s": dist([m["ttft_s"] for m in good]),
        "ttlt_s": dist([m["ttlt_s"] for m in good]),
        "prefill_tps": dist([m["prefill_tps"] for m in good], (10, 50, 90, 95)),
        "decode_tps": dist([m["decode_tps"] for m in good], (10, 50, 90, 95)),
        "tpot_s": dist([m["tpot_s"] for m in good]),
        "e2e_s": dist([m.get("e2e_s") for m in good]),
        "decode_e2e_tps": dist([m.get("decode_e2e_tps") for m in good], (10, 50, 90, 95)),
        "itl_s": dist(itl),
        "tokens_per_chunk": round(out_tokens / chunks, 3) if chunks else None,
        "throughput": {
            "wall_s": round(wall, 6) if wall else None,
            "output_tps": round(out_tokens / wall, 3) if wall else None,
            "total_tps": round((out_tokens + in_tokens) / wall, 3) if wall else None,
            "requests_per_s": round(len(good) / wall, 4) if wall else None,
        },
        "text_kinds": sorted({k for r, _ in per for k in r.get("text_kinds", [])}),
        "answering": sorted({"|".join(f"{k}={v}" for k, v in sorted((r.get("headers") or {}).items())
                                      if k.startswith("x-mllm") and k != "x-mllm-timing") or "-"
                             for r, _ in per}),
    }


def mllm_timing_summary(measured):
    """Per-phase distributions (seconds) of the router's per-request timing comments."""
    per_phase = {}
    for rec in measured:
        timing = rec.get("mllm_timing") or {}
        if rec.get("status") != 200:
            continue
        for key, value in timing.items():
            if key.endswith("_ms") and isinstance(value, (int, float)):
                per_phase.setdefault(key[:-3] + "_s", []).append(value / 1000.0)
    return {k: dist(v) for k, v in sorted(per_phase.items())}


# ---------------------------------------------------------------------------- mllm latency view

def latency_instances(doc):
    """The latency instances of one deployment from a status view or the API report."""
    if not isinstance(doc, dict):
        return []
    for view in (doc, doc.get("deployment") or {}):
        if isinstance(view.get("latency"), list):
            return view["latency"]
    out = []
    for dep in doc.get("deployments") or []:
        out.extend(dep.get("instances") or [])
    return out


def _series_totals(instances):
    """{name: {tier, source, count, sum, buckets {le: count}}}, summed over instances."""
    out = {}
    for inst in instances:
        for s in inst.get("series") or []:
            e = out.setdefault(s["name"], {"tier": s.get("tier"), "source": s.get("source"),
                                           "count": 0, "sum": 0.0, "buckets": {}, "engine": inst.get("engine")})
            e["count"] += s.get("count") or 0
            e["sum"] += s.get("sum_seconds") or 0.0
            for b in s.get("buckets") or []:
                le = "+Inf" if b.get("le") is None else repr(float(b["le"]))
                e["buckets"][le] = e["buckets"].get(le, 0) + (b.get("count") or 0)
    return out


def latencydelta(before_doc, after_doc):
    """Window of the mllm latency view between two reads: per series count, mean and bucketed p50/p95/p99."""
    before = _series_totals(latency_instances(before_doc))
    after = _series_totals(latency_instances(after_doc))
    out = {}
    for name, a in sorted(after.items()):
        b = before.get(name, {"count": 0, "sum": 0.0, "buckets": {}})
        count = a["count"] - b["count"]
        if count <= 0:
            continue
        per_bucket = {le: c - b["buckets"].get(le, 0) for le, c in a["buckets"].items()}
        # Non-cumulative -> cumulative for bucket_pct.
        cumulative, running = {}, 0
        for le in sorted(per_bucket, key=lambda x: math.inf if x == "+Inf" else float(x)):
            running += per_bucket[le]
            cumulative[le] = running
        dsum = a["sum"] - b["sum"]
        entry = {"tier": a["tier"], "source": a["source"], "engine": a.get("engine"), "count": count,
                 "sum": round(dsum, 6), "mean": round(dsum / count, 6)}
        for q in (50, 95, 99):
            v = bucket_pct(cumulative, q)
            entry[f"p{q}_bucketed"] = round(v, 6) if v is not None else None
        out[name] = entry
    return out


def _mean(series, name):
    s = series.get(name)
    return s["mean"] if s and s.get("count") else None


def _diff(a, b):
    return round(a - b, 6) if a is not None and b is not None else None


def path_overhead(series, cell=None):
    """Where time goes between the client and the engine, from means of one window.

    client_ttft - router first content    client <-> router (loopback, SSE framing)
    router pre-forward                     queue, selection and lease grant in the router
    router upstream first byte - ingress   router -> host ingress: network, TLS and the
      first byte                           host agent before its ingress clock starts
    ingress first byte - engine TTFT       ingress -> engine on loopback beyond the
                                           engine's own time to first token
    client TTFT - engine TTFT              the whole mllm path at first token
    Engine TTFT is the engine's own histogram (`source: engine`) when it exposes one.
    """
    client_ttft = ((cell or {}).get("ttft_s") or {}).get("mean")
    client_ttlt = ((cell or {}).get("ttlt_s") or {}).get("mean")
    engine_ttft = _mean(series, "engine_time_to_first_token")
    engine_e2e = _mean(series, "engine_e2e_request_latency")
    return {
        "engine_ttft_source": (series.get("engine_time_to_first_token") or {}).get("source"),
        "client_to_router_ttft_mean_s": _diff(client_ttft, _mean(series, "router_time_to_first_content")),
        "router_pre_forward_mean_s": _mean(series, "router_pre_forward"),
        "router_to_ingress_first_byte_mean_s": _diff(_mean(series, "router_upstream_first_byte"),
                                                     _mean(series, "ingress_time_to_first_byte")),
        "ingress_to_engine_ttft_mean_s": _diff(_mean(series, "ingress_time_to_first_byte"), engine_ttft),
        "path_overhead_ttft_mean_s": _diff(client_ttft, engine_ttft),
        "path_overhead_e2e_mean_s": _diff(client_ttlt, engine_e2e),
    }


def read_json(path):
    with open(path) as handle:
        return json.load(handle)


def cmd_latencydelta(args):
    series = latencydelta(read_json(args.before), read_json(args.after))
    out = {"series": series}
    if args.cell:
        out["overhead"] = path_overhead(series, read_json(args.cell))
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


# ---------------------------------------------------------------------------- once (lifecycle)

def cmd_once(args):
    prompt, words = make_prompt(args.seed, f"life-{args.label}", 0, args.prompt_tokens)
    rec = stream_chat(args.route, prompt, max_tokens=args.max_tokens, ignore_eos=False, base=args.base,
                      timeout=args.timeout)
    m = derive(rec, words + 60)
    out = {"label": args.label, "route": args.route, "t_send_unix_ms": rec["t_send_unix_ms"],
           "status": rec.get("status"), "ok": m["ok"], "ttft_s": m.get("ttft_s"), "ttlt_s": m.get("ttlt_s"),
           "t_first_token_unix_ms": (rec["t_send_unix_ms"] + int(round(m["ttft_s"] * 1000)))
           if m.get("ttft_s") is not None else None,
           "completion_tokens": m.get("completion_tokens"), "usage_source": m.get("usage_source"),
           "headers": rec.get("headers"), "error": (rec.get("error_text") or rec.get("transport_error") or "")[:300]}
    with open(args.out, "a") as handle:
        handle.write(json.dumps(out, sort_keys=True) + "\n")
    print(json.dumps(out, sort_keys=True))
    return 0 if m["ok"] else 1


# ---------------------------------------------------------------------------- tcprtt

def cmd_tcprtt(args):
    result = {}
    for target in args.target:
        name, hostport = target.split("=", 1)
        host, port = hostport.rsplit(":", 1)
        samples, errors = [], []
        for _ in range(args.samples):
            t = time.monotonic()
            try:
                with socket.create_connection((host, int(port)), timeout=5):
                    samples.append(time.monotonic() - t)
            except OSError as error:
                errors.append(f"{type(error).__name__}: {error}")
            time.sleep(0.05)
        result[name] = {"target": hostport, "connect_s": dist(samples), "errors": errors[:3]}
    write_json(args.out, result)
    print(json.dumps(result, sort_keys=True))
    return 0


# ---------------------------------------------------------------------------- prometheus deltas

PROM_LINE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+([-+0-9.eE]+|NaN|\+Inf|-Inf)")


def parse_prom(text):
    """{(name, labels-without-le): value} plus buckets {(name, labels): {le: value}}."""
    scalars, buckets = {}, {}
    for line in text.splitlines():
        m = PROM_LINE.match(line)
        if not m:
            continue
        name, labels, value = m.group(1), m.group(2) or "", m.group(3)
        try:
            value = float(value)
        except ValueError:
            continue
        pairs = dict(re.findall(r'(\w+)="((?:[^"\\]|\\.)*)"', labels))
        le = pairs.pop("le", None)
        key = (name, tuple(sorted(pairs.items())))
        if name.endswith("_bucket") and le is not None:
            buckets.setdefault(key, {})[le] = value
        else:
            scalars[key] = value
    return scalars, buckets


def _sum_by_name(table):
    out = {}
    for (name, _), value in table.items():
        out[name] = out.get(name, 0.0) + value
    return out


def bucket_pct(cumulative, q):
    """Percentile from cumulative bucket counts {le: count}, linear within a bucket."""
    items = sorted(((math.inf if le == "+Inf" else float(le)), c) for le, c in cumulative.items())
    if not items or items[-1][1] <= 0:
        return None
    rank = items[-1][1] * q / 100.0
    prev_le, prev_c = 0.0, 0.0
    for le, c in items:
        if c >= rank:
            if le == math.inf:
                return prev_le
            span = c - prev_c
            return prev_le + (le - prev_le) * ((rank - prev_c) / span if span > 0 else 0)
        prev_le, prev_c = le, c
    return prev_le


def promdelta(before_text, after_text, prefixes=("sglang:", "vllm:")):
    b_s, b_b = parse_prom(before_text)
    a_s, a_b = parse_prom(after_text)
    bs, as_ = _sum_by_name(b_s), _sum_by_name(a_s)
    out = {"histograms": {}, "counters": {}}
    names = {n[: -len("_count")] for n in as_ if n.endswith("_count") and n.startswith(prefixes)}
    for base in sorted(names):
        dcount = as_.get(base + "_count", 0) - bs.get(base + "_count", 0)
        dsum = as_.get(base + "_sum", 0) - bs.get(base + "_sum", 0)
        cum = {}
        for (name, labels), table in a_b.items():
            if name != base + "_bucket":
                continue
            before = b_b.get((name, labels), {})
            for le, c in table.items():
                cum[le] = cum.get(le, 0.0) + c - before.get(le, 0.0)
        entry = {"count": dcount, "sum": round(dsum, 6), "mean": round(dsum / dcount, 6) if dcount > 0 else None}
        for q in (50, 95, 99):
            v = bucket_pct(cum, q) if dcount > 0 else None
            entry[f"p{q}_bucketed"] = round(v, 6) if v is not None else None
        out["histograms"][base] = entry
    for name in sorted(as_):
        if name.startswith(prefixes) and name.endswith("_total"):
            out["counters"][name] = as_[name] - bs.get(name, 0.0)
    return out


def read_prom(path):
    """A scrape file as the row writes it: body, then '# HTTP <code>'."""
    with open(path, errors="replace") as handle:
        text = handle.read()
    m = re.search(r"^# HTTP (\d+)", text, re.M)
    return text, (int(m.group(1)) if m else None)


def cmd_promdelta(args):
    before, _ = read_prom(args.before)
    after, _ = read_prom(args.after)
    print(json.dumps(promdelta(before, after), indent=1, sort_keys=True))
    return 0


# ---------------------------------------------------------------------------- report

ENGINE_TTFT = ("sglang:time_to_first_token_seconds", "vllm:time_to_first_token_seconds")
ENGINE_E2E = ("sglang:e2e_request_latency_seconds", "vllm:e2e_request_latency_seconds")


def first_hist(delta, names):
    for n in names:
        h = delta["histograms"].get(n)
        if h and h["count"]:
            return h
    return None


def fixture_facts(fixture_path, name):
    facts = {"fixture": name}
    m = re.match(r"^([vs])([ab])-(\w+)", name or "")
    if m:
        facts.update({"engine": {"v": "vllm", "s": "sglang"}[m.group(1)],
                      "host": {"a": os.environ.get("HOST_A", "host-a"), "b": os.environ.get("HOST_B", "host-b")}[m.group(2)], "model": m.group(3)})
    if fixture_path and os.path.exists(fixture_path):
        with open(fixture_path) as handle:
            doc = json.load(handle)
        cfg = doc.get("engine_config", {})
        facts.update({"deployment": doc.get("name"), "residency": doc.get("residency"),
                      "checkpoint": (doc.get("model") or {}).get("source", {}).get("path"),
                      "content_fingerprint": (doc.get("model") or {}).get("content_fingerprint"),
                      "engine_config": {k: cfg.get(k) for k in ("dtype", "context_length", "max_concurrent_requests",
                                                                 "cuda_graphs", "quantization", "kv_cache_dtype",
                                                                 "memory", "extra_args")}})
    return facts


def cmd_report(args):
    evid = args.evid
    cells = []
    for path in sorted(glob.glob(os.path.join(evid, "cells", "*.json"))):
        if path.endswith(".warmup.json"):
            continue
        with open(path) as handle:
            cell = json.load(handle)
        tag = cell["cell"]
        before = os.path.join(evid, "metrics", f"{tag}.before.prom")
        after = os.path.join(evid, "metrics", f"{tag}.after.prom")
        if os.path.exists(before) and os.path.exists(after):
            b_text, b_code = read_prom(before)
            a_text, a_code = read_prom(after)
            engine = {"http": [b_code, a_code]}
            if b_code == 200 and a_code == 200:
                d = promdelta(b_text, a_text)
                engine["delta"] = d
                ttft, e2e = first_hist(d, ENGINE_TTFT), first_hist(d, ENGINE_E2E)
                engine["engine_ttft_mean_s"] = ttft["mean"] if ttft else None
                engine["engine_e2e_mean_s"] = e2e["mean"] if e2e else None
                engine["engine_request_count"] = ttft["count"] if ttft else None
                if ttft and cell["ttft_s"]["mean"] is not None:
                    engine["path_overhead_ttft_mean_s"] = round(cell["ttft_s"]["mean"] - ttft["mean"], 6)
                if e2e and cell["ttlt_s"]["mean"] is not None:
                    engine["path_overhead_e2e_mean_s"] = round(cell["ttlt_s"]["mean"] - e2e["mean"], 6)
            else:
                engine["note"] = ("engine /metrics not readable without a secret (vLLM keys it) or not enabled; "
                                  "no direct baseline for this cell")
            cell["engine_side"] = engine
        # mllm latency view window (router, ingress, engine tiers) when the row saved one.
        l_before = os.path.join(evid, "latency", f"{tag}.before.json")
        l_after = os.path.join(evid, "latency", f"{tag}.after.json")
        if os.path.exists(l_before) and os.path.exists(l_after):
            try:
                series = latencydelta(read_json(l_before), read_json(l_after))
                cell["mllm_side"] = {"series": series, "overhead": path_overhead(series, cell)}
            except (ValueError, KeyError, TypeError) as error:
                cell["mllm_side"] = {"error": f"{type(error).__name__}: {error}"}
        cells.append(cell)
    lifecycle = []
    life = os.path.join(evid, "lifecycle.jsonl")
    if os.path.exists(life):
        with open(life) as handle:
            lifecycle = [json.loads(line) for line in handle if line.strip()]
    life_summary = {}
    for rec in lifecycle:
        kind = re.sub(r"-\d+$", "", rec["label"])
        life_summary.setdefault(kind, []).append(rec.get("ttft_s") if rec.get("ok") else None)
    life_summary = {k: {"samples_s": v, **dist([x for x in v if x is not None], (50, 95)),
                        "failed": sum(1 for x in v if x is None)} for k, v in life_summary.items()}
    extras = {}
    p = os.path.join(evid, "netfloor.json")
    if os.path.exists(p):
        with open(p) as handle:
            extras["netfloor"] = json.load(handle)
    # Page-cache samples, switch states and CLI marks, kept verbatim.
    for name in ("pagecache.txt", "switch-states.txt", "marks.txt", "times.txt"):
        p = os.path.join(evid, name)
        if os.path.exists(p):
            with open(p) as handle:
                extras[name[:-4].replace("-", "_")] = [line.rstrip("\n") for line in handle if line.strip()]
    facts = fixture_facts(os.path.join(evid, "fixture-bn.json"), args.fixture or "")
    report = {"row": "M80", **facts, "cells": cells, "lifecycle": lifecycle, "lifecycle_summary": life_summary,
              **extras,
              "note": ("Measured through the shipped router on control-host. TTFT includes router, ingress, agent and "
                       "network time. Not a product guarantee (SPEC section 17). CPU or fake runs are not evidence.")}
    write_json(os.path.join(evid, "bench.json"), report)
    with open(os.path.join(evid, "summary.md"), "w") as handle:
        handle.write(render_markdown(report))
    print(render_markdown(report))
    return 0


def fmt(v, scale=1.0, digits=1):
    return "-" if v is None else f"{v * scale:.{digits}f}"


def render_markdown(r):
    lines = [f"# M80 {r.get('fixture', '')}", "",
             f"Engine {r.get('engine', '?')}, host {r.get('host', '?')}, model {r.get('model', '?')}, "
             f"deployment {r.get('deployment', '?')}, residency {r.get('residency', '?')}.", "",
             "Times in ms; rates in tokens/s. TTFT through the router. `ovh` is client mean TTFT minus the "
             "engine's own mean TTFT from /metrics (SGLang only).", "",
             "| prompt | C | n | TTFT p50 | TTFT p95 | TTLT p50 | prefill p50 | decode p50 | ITL p50 | ITL p95 "
             "| ITL p99 | out tok/s | req/s | ovh ms | usage |",
             "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
    for c in r["cells"]:
        eng = c.get("engine_side", {})
        lines.append("| {p} | {C} | {n}/{N} | {a} | {b} | {d} | {e} | {f} | {g} | {h} | {i} | {j} | {k} | {o} | {u} |".format(
            p=fmt((c.get("prompt_tokens") or {}).get("p50"), 1, 0), C=c["concurrency"], n=c["ok"],
            N=c["requests"], a=fmt(c["ttft_s"]["p50"], 1000), b=fmt(c["ttft_s"]["p95"], 1000),
            d=fmt(c["ttlt_s"]["p50"], 1000), e=fmt(c["prefill_tps"]["p50"]), f=fmt(c["decode_tps"]["p50"]),
            g=fmt(c["itl_s"]["p50"], 1000, 2), h=fmt(c["itl_s"]["p95"], 1000, 2), i=fmt(c["itl_s"]["p99"], 1000, 2),
            j=fmt(c["throughput"]["output_tps"]), k=fmt(c["throughput"]["requests_per_s"], 1, 2),
            o=fmt(eng.get("path_overhead_ttft_mean_s"), 1000, 2), u=",".join(c.get("usage_source", []))))
    if r.get("lifecycle_summary"):
        lines += ["", "| lifecycle to first token | n | failed | p50 ms | mean ms | samples ms |", "|---|---:|---:|---:|---:|---|"]
        for kind, s in r["lifecycle_summary"].items():
            lines.append(f"| {kind} | {s['n']} | {s['failed']} | {fmt(s['p50'], 1000, 0)} | {fmt(s['mean'], 1000, 0)} | "
                         + ", ".join(fmt(x, 1000, 0) for x in s["samples_s"]) + " |")
    mllm_cells = [c for c in r["cells"] if (c.get("mllm_side") or {}).get("overhead")]
    if mllm_cells:
        lines += ["", "Path overhead from the mllm latency view (means, ms): client->router, router pre-forward, "
                  "router->ingress, ingress->engine TTFT, whole path at TTFT and at the last token. Engine TTFT "
                  "source per cell.", "",
                  "| prompt | C | client->router | pre-forward | router->ingress | ingress->engine | path TTFT "
                  "| path e2e | engine TTFT from |",
                  "|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
        for c in mllm_cells:
            o = c["mllm_side"]["overhead"]
            lines.append("| {p} | {C} | {a} | {b} | {d} | {e} | {f} | {g} | {h} |".format(
                p=fmt((c.get("prompt_tokens") or {}).get("p50"), 1, 0), C=c["concurrency"],
                a=fmt(o.get("client_to_router_ttft_mean_s"), 1000, 2), b=fmt(o.get("router_pre_forward_mean_s"), 1000, 2),
                d=fmt(o.get("router_to_ingress_first_byte_mean_s"), 1000, 2),
                e=fmt(o.get("ingress_to_engine_ttft_mean_s"), 1000, 2),
                f=fmt(o.get("path_overhead_ttft_mean_s"), 1000, 2), g=fmt(o.get("path_overhead_e2e_mean_s"), 1000, 2),
                h=o.get("engine_ttft_source") or "-"))
    nf = r.get("netfloor")
    if nf:
        lines += ["", "Network floor (TCP connect, no request sent): " + "; ".join(
            f"{k} p50 {fmt(v['connect_s']['p50'], 1000, 2)} ms" for k, v in nf.items())]
    lines += ["", r["note"], ""]
    return "\n".join(lines)


def write_json(path, obj):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w") as handle:
        json.dump(obj, handle, indent=1, sort_keys=True)
        handle.write("\n")


# ---------------------------------------------------------------------------- main

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--base", default=matrixhttp.DEFAULT_BASE)
    common.add_argument("--timeout", type=float, default=900)
    common.add_argument("--seed", type=int, default=8080)

    run = sub.add_parser("run", parents=[common])
    run.add_argument("--route", required=True)
    run.add_argument("--prompt-tokens", type=int, required=True)
    run.add_argument("--concurrency", type=int, required=True)
    run.add_argument("--repeats", type=int, default=3, help="measured requests per worker")
    run.add_argument("--warmup", type=int, default=1, help="warmup requests per worker (excluded)")
    run.add_argument("--max-tokens", type=int, default=256)
    run.add_argument("--context-length", type=int, default=32768)
    run.add_argument("--calibration", help="a --warmup-only cell file; skips warmup")
    run.add_argument("--warmup-only", action="store_true")
    run.add_argument("--no-ignore-eos", action="store_true")
    run.add_argument("--out", required=True)
    run.add_argument("--records")

    once = sub.add_parser("once", parents=[common])
    once.add_argument("--route", required=True)
    once.add_argument("--label", required=True)
    once.add_argument("--prompt-tokens", type=int, default=128)
    once.add_argument("--max-tokens", type=int, default=32)
    once.add_argument("--out", required=True)

    tcp = sub.add_parser("tcprtt")
    tcp.add_argument("--target", action="append", required=True, help="NAME=HOST:PORT")
    tcp.add_argument("--samples", type=int, default=20)
    tcp.add_argument("--out", required=True)

    pd = sub.add_parser("promdelta")
    pd.add_argument("--before", required=True)
    pd.add_argument("--after", required=True)

    ld = sub.add_parser("latencydelta")
    ld.add_argument("--before", required=True, help="status/inspect JSON or latency API report")
    ld.add_argument("--after", required=True)
    ld.add_argument("--cell", help="the cell JSON, for the client-side overhead terms")

    rep = sub.add_parser("report")
    rep.add_argument("--evid", required=True)
    rep.add_argument("--fixture")

    args = parser.parse_args()
    return {"run": cmd_run, "once": cmd_once, "tcprtt": cmd_tcprtt, "promdelta": cmd_promdelta,
            "latencydelta": cmd_latencydelta, "report": cmd_report}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
