#!/usr/bin/env python3
"""One streamed chat request through CapyCTL, with reasoning on and
stream_options.include_usage; prints the shape of the stream as JSON.

usage: stream_check.py CREDENTIALS_FILE MODEL [MAX_TOKENS] [ENDPOINT]
"""
import json, sys, time, urllib.request

cred, model = sys.argv[1], sys.argv[2]
max_tokens = int(sys.argv[3]) if len(sys.argv) > 3 else 1024
endpoint = sys.argv[4] if len(sys.argv) > 4 else "http://127.0.0.1:8443"
key = next(l.split(":", 1)[1].strip() for l in open(cred) if l.startswith("api_key:"))
body = {
    "model": model, "stream": True, "stream_options": {"include_usage": True},
    "max_tokens": max_tokens, "temperature": 0,
    "messages": [{"role": "user", "content": "What is 17 * 23? Think it through, then give the number."}],
}
req = urllib.request.Request(f"{endpoint}/v1/chat/completions", data=json.dumps(body).encode(),
                             headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
t0 = time.perf_counter()
out = {"chunks": 0, "reasoning_chunks": 0, "content_chunks": 0, "usage_chunks": [], "finish_reason": None,
       "done": False, "first_token_s": None, "reasoning_tail": "", "content_tail": ""}
reasoning, content = [], []
with urllib.request.urlopen(req, timeout=1800) as resp:
    out["http_status"] = resp.status
    for raw in resp:
        line = raw.strip()
        if not line.startswith(b"data:"):
            continue
        data = line[5:].strip()
        if data == b"[DONE]":
            out["done"] = True
            break
        obj = json.loads(data)
        out["chunks"] += 1
        choices = obj.get("choices") or []
        if obj.get("usage"):
            out["usage_chunks"].append({"index": out["chunks"], "choices_len": len(choices), "usage": obj["usage"]})
        for ch in choices:
            d = ch.get("delta") or {}
            r = d.get("reasoning_content") or d.get("reasoning")
            if r:
                out["reasoning_chunks"] += 1
                reasoning.append(r)
            if d.get("content"):
                out["content_chunks"] += 1
                content.append(d["content"])
            if (r or d.get("content")) and out["first_token_s"] is None:
                out["first_token_s"] = round(time.perf_counter() - t0, 3)
            if ch.get("finish_reason"):
                out["finish_reason"] = ch["finish_reason"]
out["total_s"] = round(time.perf_counter() - t0, 3)
out["reasoning_tail"] = "".join(reasoning)[-160:]
out["content_tail"] = "".join(content)[-160:]
out["answer_ok"] = "391" in "".join(content)
print(json.dumps(out, indent=1))
