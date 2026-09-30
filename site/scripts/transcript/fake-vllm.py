"""A stand-in for `vllm` used only to capture the documentation's command
output on a machine without an engine. It answers the HTTP surface capyctl
drives (health, models, streaming chat, sleep and wake) and loads nothing.

This is not qualification of any engine recipe: passing through it shows
only what capyctl's own commands print.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "0.29.0"


def arg(argv, name, default=None):
    for i, a in enumerate(argv):
        if a == name and i + 1 < len(argv):
            return argv[i + 1]
        if a.startswith(name + "="):
            return a.split("=", 1)[1]
    return default


def serve(argv):
    host = arg(argv, "--host", "127.0.0.1")
    port = int(arg(argv, "--port", "8000"))
    model = argv[0] if argv and not argv[0].startswith("-") else arg(argv, "--model", "model")
    served = arg(argv, "--served-model-name", model)
    state = {"sleeping": False}
    # A real engine runs a worker process in its process group; capyctl records
    # the whole group, so the stand-in keeps one idle child beside it.
    import subprocess
    subprocess.Popen([sys.executable, "-c", "import time\nwhile True: time.sleep(3600)"])

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            pass

        def send(self, code, body):
            data = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
            self.close_connection = True

        def body(self):
            if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
                raw = b""
                while True:
                    size = int(self.rfile.readline().strip() or b"0", 16)
                    if size == 0:
                        self.rfile.readline()
                        break
                    raw += self.rfile.read(size)
                    self.rfile.readline()
            else:
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n) if n else b""
            try:
                return json.loads(raw or b"{}")
            except ValueError:
                return {}

        def do_GET(self):
            path = self.path.split("?")[0]
            if path == "/health":
                self.send(200, {})
            elif path == "/v1/models":
                self.send(200, {"object": "list", "data": [
                    {"id": served, "object": "model", "created": int(time.time()), "owned_by": "vllm"}]})
            elif path == "/is_sleeping":
                self.send(200, {"is_sleeping": state["sleeping"]})
            elif path == "/metrics":
                data = (f'vllm:num_requests_running{{model_name="{served}"}} 0.0\n'
                        f'vllm:num_requests_waiting{{model_name="{served}"}} 0.0\n').encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)
                self.close_connection = True
            elif path == "/version":
                self.send(200, {"version": VERSION})
            else:
                self.send(404, {"error": "not found"})

        def do_POST(self):
            path = self.path.split("?")[0]
            request = self.body()
            if path == "/sleep":
                state["sleeping"] = True
                self.send(200, {})
            elif path == "/wake_up":
                state["sleeping"] = False
                self.send(200, {})
            elif path in ("/collective_rpc", "/reset_prefix_cache"):
                self.send(200, {"success": True, "results": []})
            elif path == "/v1/chat/completions":
                self.chat(request)
            else:
                self.send(404, {"error": "not found"})

        def chat(self, request):
            words = ["Hello!", " How", " can", " I", " help", " you", " today?"]
            created = int(time.time())

            def chunk(delta, finish=None):
                return {"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": created,
                        "model": served, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}

            frames = [chunk({"role": "assistant", "content": ""})]
            frames += [chunk({"content": w}) for w in words]
            last = chunk({}, "stop")
            if (request.get("stream_options") or {}).get("include_usage"):
                frames.append(last)
                frames.append({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": created,
                               "model": served, "choices": [],
                               "usage": {"prompt_tokens": 9, "completion_tokens": len(words),
                                         "total_tokens": 9 + len(words)}})
            else:
                frames.append(last)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            for frame in frames + ["[DONE]"]:
                text = frame if isinstance(frame, str) else json.dumps(frame)
                data = f"data: {text}\n\n".encode()
                self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            self.close_connection = True

    ThreadingHTTPServer((host, port), Handler).serve_forever()


def main():
    argv = sys.argv[1:]
    if not argv or argv[0] in ("--version", "-v", "version"):
        print(VERSION)
        return
    if argv[0] == "serve":
        serve(argv[1:])
        return
    print(VERSION)


if __name__ == "__main__":
    main()
