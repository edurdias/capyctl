"""Shared HTTP client for the matrix harness (plan unit W2).

Talks only to the server's loopback inference listener; clients never reach a
host agent or engine directly. The API key comes from CAPYCTL_API_KEY, which
lib.sh fills from server-credentials.json; it is never printed or recorded.
Standard library only.
"""

import http.client
import json
import os
import time
import urllib.parse
import uuid

DEFAULT_BASE = os.environ.get("CAPYCTL_INFER_URL", "http://127.0.0.1:8443")
# Response headers recorded as evidence; anything credential-shaped is dropped.
_DROP = ("authorization", "cookie", "set-cookie", "x-api-key")


def api_key():
    key = os.environ.get("CAPYCTL_API_KEY")
    if not key:
        raise SystemExit("CAPYCTL_API_KEY is not set (lib.sh load_api_key)")
    return key


def _headers(response):
    return {k.lower(): v for k, v in response.getheaders() if k.lower() not in _DROP}


def chat(route, messages, *, stream=False, max_tokens=32, temperature=0.0, extra=None,
         base=DEFAULT_BASE, timeout=900.0, marker=None):
    """One chat completion. Returns an evidence record; never raises on HTTP errors."""
    marker = marker or uuid.uuid4().hex
    body = {"model": route, "messages": messages, "max_tokens": max_tokens,
            "temperature": temperature, "stream": stream}
    if extra:
        body.update(extra)
    parsed = urllib.parse.urlparse(base)
    record = {"marker": marker, "route": route, "stream": stream, "max_tokens": max_tokens,
              "started_unix_ms": int(time.time() * 1000)}
    start = time.monotonic()
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout)
    try:
        conn.request("POST", "/v1/chat/completions", body=json.dumps(body).encode(),
                     headers={"Authorization": "Bearer " + api_key(), "Content-Type": "application/json",
                              "x-request-id": marker})
        response = conn.getresponse()
        record["status"] = response.status
        record["headers"] = _headers(response)
        if stream and response.status == 200:
            _read_sse(response, record, start)
        else:
            raw = response.read()
            record["body_bytes"] = len(raw)
            try:
                payload = json.loads(raw)
                record["model"] = payload.get("model")
                choice = (payload.get("choices") or [{}])[0]
                record["content"] = (choice.get("message") or {}).get("content")
                record["finish_reason"] = choice.get("finish_reason")
                record["usage"] = payload.get("usage")
                record["logprobs"] = choice.get("logprobs")
                if response.status != 200:
                    record["error"] = payload.get("error", payload)
            except ValueError:
                record["error_text"] = raw[:400].decode(errors="replace")
    except (OSError, http.client.HTTPException) as error:
        record["status"] = None
        record["transport_error"] = f"{type(error).__name__}: {error}"
    finally:
        conn.close()
    record["elapsed_s"] = round(time.monotonic() - start, 4)
    record["finished_unix_ms"] = int(time.time() * 1000)
    return record


def _read_sse(response, record, start):
    """Parse an SSE stream and judge its framing: every event is `data: <json>`,
    events are separated by blank lines, and the stream ends with `data: [DONE]`."""
    content, events, malformed, done, first, comments = [], 0, 0, False, None, 0
    models = set()
    buffer = b""
    while True:
        chunk = response.read1(65536) if hasattr(response, "read1") else response.read(65536)
        if not chunk:
            break
        buffer = (buffer + chunk).replace(b"\r\n", b"\n")
        while b"\n\n" in buffer:
            event, buffer = buffer.split(b"\n\n", 1)
            event = event.strip(b"\r\n")
            if not event:
                continue
            # SSE comments (`:` lines: the router's keep-alive during a stall,
            # its timing line) are legal framing, counted, never content.
            # Found live 2026-09-23 (M58): a stalled stream's keep-alive was
            # judged malformed.
            if all(line.startswith(b":") for line in event.split(b"\n")):
                comments += 1
                continue
            if not event.startswith(b"data: "):
                malformed += 1
                continue
            data = event[6:]
            events += 1
            if data == b"[DONE]":
                done = True
                continue
            try:
                payload = json.loads(data)
            except ValueError:
                malformed += 1
                continue
            if first is None:
                first = time.monotonic() - start
            models.add(payload.get("model"))
            for choice in payload.get("choices") or []:
                piece = (choice.get("delta") or {}).get("content")
                if piece:
                    content.append(piece)
                if choice.get("finish_reason"):
                    record["finish_reason"] = choice["finish_reason"]
    if buffer.strip():
        malformed += 1
    record.update({"content": "".join(content), "sse_events": events, "sse_malformed": malformed, "sse_comments": comments,
                   "sse_done": done, "sse_well_formed": malformed == 0 and done,
                   "ttft_s": round(first, 4) if first is not None else None,
                   "model": sorted(m for m in models if m)[0] if models - {None} else None})


def append_jsonl(path, record):
    with open(path, "a") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")
