#!/usr/bin/env python3
"""Engine invalid-request rejections through the router (SPEC section 10; T17, T19).

usage: reject.py <route> <count>

Sends <count> non-streaming requests the engine rejects as invalid
(`tool_choice: auto` on an engine launched without a tool parser), then one
streaming request. The owner's rule (2026-09-24): a complete engine 400, 413 or
422 with a JSON body is completion evidence, so each answer must be a 400
`engine_rejected` and a streamed rejection must not end as a successful stream.
Prints status counts and bounded samples. The API key is read from MLLM_API_KEY
only and never recorded. Standard library only.
"""

import http.client
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrixhttp  # noqa: E402

TOOLS = [{"type": "function", "function": {
    "name": "get_weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]


def send(route, stream):
    body = {"model": route, "messages": [{"role": "user", "content": "weather in Lisbon?"}], "max_tokens": 32,
            "tools": TOOLS, "tool_choice": "auto", "stream": stream}
    conn = http.client.HTTPConnection("127.0.0.1", 8443, timeout=120)
    conn.request("POST", "/v1/chat/completions", body=json.dumps(body).encode(),
                 headers={"Authorization": "Bearer " + matrixhttp.api_key(), "Content-Type": "application/json"})
    response = conn.getresponse()
    return response.status, response.read().decode(errors="replace")


def main():
    route, count = sys.argv[1], int(sys.argv[2])
    counts, sample = {}, None
    for _ in range(count):
        status, body = send(route, False)
        counts[status] = counts.get(status, 0) + 1
        sample = sample or body[:300]
    stream_status, stream_body = send(route, True)
    print(json.dumps({"nonstream_status": counts, "sample": sample,
                      "stream_status": stream_status, "stream_body": stream_body[:300]}))
    ok = (counts == {400: count} and "engine_rejected" in (sample or "")
          and "engine_rejected" in stream_body and "[DONE]" not in stream_body)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
