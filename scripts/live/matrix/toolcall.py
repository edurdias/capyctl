#!/usr/bin/env python3
"""One routed tool-call request (SPEC section 10: tool calls pass the allowlist; T19).

usage: toolcall.py --route R --out FILE.jsonl [--choice named|auto] [--stream]

Sends a chat request with one `get_weather` tool through the server's router
and records status, finish reason, the tool calls returned and, when streamed,
the SSE framing. Exit 0 only when a `get_weather` call with a `city` argument
comes back. The engine must be launched with its tool parser (vLLM
`--enable-auto-tool-choice --tool-call-parser hermes`, SGLang
`--tool-call-parser qwen25`) for `auto`; see rows/TC.sh. The API key is read
from CAPYCTL_API_KEY only and never recorded. Standard library only.
"""

import argparse
import http.client
import json
import os
import sys
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrixhttp  # noqa: E402

TOOLS = [{"type": "function", "function": {
    "name": "get_weather", "description": "Current weather for a city",
    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]


def read_stream(response, record):
    """Fold streamed tool-call deltas by index, as a client SDK would."""
    calls, events, malformed, done, buffer = {}, 0, 0, False, b""
    while True:
        chunk = response.read1(65536)
        if not chunk:
            break
        buffer = (buffer + chunk).replace(b"\r\n", b"\n")
        while b"\n\n" in buffer:
            event, buffer = buffer.split(b"\n\n", 1)
            event = event.strip(b"\n")
            if not event or all(line.startswith(b":") for line in event.split(b"\n")):
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
            if "error" in payload:
                record["error"] = payload["error"]
            for choice in payload.get("choices") or []:
                if choice.get("finish_reason"):
                    record["finish_reason"] = choice["finish_reason"]
                for delta in (choice.get("delta") or {}).get("tool_calls") or []:
                    call = calls.setdefault(delta.get("index"), {"id": None, "type": "function",
                                                                 "function": {"name": "", "arguments": ""}})
                    call["id"] = call["id"] or delta.get("id")
                    function = delta.get("function") or {}
                    call["function"]["name"] = call["function"]["name"] or (function.get("name") or "")
                    call["function"]["arguments"] += function.get("arguments") or ""
    record.update({"tool_calls": list(calls.values()) or None, "sse_events": events,
                   "sse_malformed": malformed, "sse_done": done})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--route", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--choice", default="named", choices=["named", "auto"])
    parser.add_argument("--stream", action="store_true")
    args = parser.parse_args()
    body = {"model": args.route, "max_tokens": 256, "temperature": 0.0, "stream": args.stream, "tools": TOOLS,
            "messages": [{"role": "user", "content": "What is the weather in Lisbon? Use the tool."}],
            "tool_choice": ({"type": "function", "function": {"name": "get_weather"}}
                            if args.choice == "named" else "auto")}
    marker = uuid.uuid4().hex
    record = {"marker": marker, "route": args.route, "choice": args.choice, "stream": args.stream}
    start = time.monotonic()
    conn = http.client.HTTPConnection("127.0.0.1", 8443, timeout=600)
    conn.request("POST", "/v1/chat/completions", body=json.dumps(body).encode(),
                 headers={"Authorization": "Bearer " + matrixhttp.api_key(),
                          "Content-Type": "application/json", "x-request-id": marker})
    response = conn.getresponse()
    record["status"] = response.status
    if args.stream and response.status == 200:
        read_stream(response, record)
    else:
        raw = response.read()
        try:
            payload = json.loads(raw)
            choice = (payload.get("choices") or [{}])[0]
            message = choice.get("message") or {}
            record.update({"tool_calls": message.get("tool_calls"), "content": message.get("content"),
                           "finish_reason": choice.get("finish_reason"),
                           # vLLM nests `error`; capyctl's own refusals are top-level.
                           "error": payload.get("error") or (
                               {k: payload.get(k) for k in ("code", "message")} if response.status != 200 else None)})
        except ValueError:
            record["error_text"] = raw[:400].decode(errors="replace")
    record["elapsed_s"] = round(time.monotonic() - start, 3)
    calls = record.get("tool_calls") or []
    ok = response.status == 200 and any(
        (call.get("function") or {}).get("name") == "get_weather"
        and "city" in ((call.get("function") or {}).get("arguments") or "") for call in calls)
    if args.stream:
        ok = ok and record.get("sse_done") and not record.get("sse_malformed")
    record["verdict"] = "ok" if ok else "failed"
    matrixhttp.append_jsonl(args.out, record)
    print(json.dumps(record))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
