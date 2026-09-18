"""Guard tests for the vLLM development-route middleware (SPEC §9.1 / T21).

Prefers Starlette's `TestClient`, matching how a real ASGI deployment drives
the middleware; falls back to a minimal stdlib-only ASGI scope/receive/send
harness when Starlette is not importable in the running interpreter, since
`mllm_vllm_guard` itself must stay import-clean with no third-party
dependency.
"""

import asyncio
import json
import os
import unittest

from runtime.mllm_vllm_guard import RequireEngineKey

try:
    from starlette.testclient import TestClient
    from starlette.websockets import WebSocketDisconnect

    HAS_STARLETTE = True
except (ImportError, RuntimeError):
    # ImportError: starlette itself is not installed.
    # RuntimeError: starlette is installed but its TestClient extra (httpx)
    # is not, which starlette.testclient raises eagerly on import.
    HAS_STARLETTE = False


async def echo_app(scope, receive, send):
    """Minimal downstream ASGI app: accepts everything with a 200/accept."""
    if scope["type"] == "http":
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.body", "body": b"ok"})
    elif scope["type"] == "websocket":
        message = await receive()
        if message["type"] == "websocket.connect":
            await send({"type": "websocket.accept"})


class _Response:
    def __init__(self, status_code, body):
        self.status_code = status_code
        self.body = body

    def json(self):
        return json.loads(self.body)


class _MinimalClient:
    """Stdlib-only ASGI test client, used when Starlette is unavailable."""

    def __init__(self, app):
        self.app = app

    def _request(self, method, path, headers):
        return asyncio.run(self._arequest(method, path, headers or {}))

    async def _arequest(self, method, path, headers):
        scope = {
            "type": "http",
            "method": method,
            "path": path,
            "headers": [
                (name.lower().encode("latin-1"), value.encode("latin-1"))
                for name, value in headers.items()
            ],
        }
        pending = [{"type": "http.request", "body": b"", "more_body": False}]

        async def receive():
            return pending.pop(0) if pending else {"type": "http.disconnect"}

        sent = []

        async def send(message):
            sent.append(message)

        await self.app(scope, receive, send)
        status = next(m["status"] for m in sent if m["type"] == "http.response.start")
        body = b"".join(
            m.get("body", b"") for m in sent if m["type"] == "http.response.body"
        )
        return _Response(status, body)

    def get(self, path, headers=None):
        return self._request("GET", path, headers)

    def post(self, path, headers=None):
        return self._request("POST", path, headers)

    def websocket_messages(self, path, headers=None):
        return asyncio.run(self._awebsocket(path, headers or {}))

    async def _awebsocket(self, path, headers):
        scope = {
            "type": "websocket",
            "path": path,
            "headers": [
                (name.lower().encode("latin-1"), value.encode("latin-1"))
                for name, value in headers.items()
            ],
        }
        pending = [{"type": "websocket.connect"}]

        async def receive():
            return pending.pop(0) if pending else {"type": "websocket.disconnect"}

        sent = []

        async def send(message):
            sent.append(message)

        await self.app(scope, receive, send)
        return sent


def make_client(app):
    return TestClient(app) if HAS_STARLETTE else _MinimalClient(app)


def websocket_accepted(client, path, headers=None):
    """True if a WebSocket connect to path is accepted, False if rejected."""
    if HAS_STARLETTE:
        try:
            with client.websocket_connect(path, headers=headers or {}):
                return True
        except WebSocketDisconnect:
            return False
    sent = client.websocket_messages(path, headers)
    return any(message["type"] == "websocket.accept" for message in sent)


class RequireEngineKeyTests(unittest.TestCase):
    def setUp(self):
        self._previous = os.environ.get("VLLM_API_KEY")
        self.addCleanup(self._restore_env)

    def _restore_env(self):
        if self._previous is None:
            os.environ.pop("VLLM_API_KEY", None)
        else:
            os.environ["VLLM_API_KEY"] = self._previous

    def test_dev_routes_require_the_engine_key(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        self.assertEqual(client.post("/sleep").status_code, 401)
        self.assertEqual(client.post("/collective_rpc").status_code, 401)
        self.assertEqual(
            client.post("/sleep", headers={"Authorization": "Bearer k3y"}).status_code,
            200,
        )
        self.assertEqual(client.get("/health").status_code, 200)
        # Belt and braces with vLLM's own guard on its inference prefixes.
        self.assertEqual(client.get("/v1/models").status_code, 401)

    def test_unauthorized_response_is_json_with_expected_body(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        response = client.post("/wake_up")
        self.assertEqual(response.status_code, 401)
        self.assertEqual(response.json(), {"error": "Unauthorized"})

    def test_wrong_key_is_rejected(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        response = client.post(
            "/is_sleeping", headers={"Authorization": "Bearer not-the-key"}
        )
        self.assertEqual(response.status_code, 401)

    def test_options_passes_without_a_key(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        # Starlette's TestClient has no direct OPTIONS verb helper; use the
        # minimal client's generic request for both backends.
        response = _MinimalClient(RequireEngineKey(echo_app))._request(
            "OPTIONS", "/collective_rpc", {}
        )
        self.assertEqual(response.status_code, 200)

    def test_missing_key_env_refuses_everything(self):
        os.environ.pop("VLLM_API_KEY", None)
        with self.assertRaises(RuntimeError):
            RequireEngineKey(echo_app)

    def test_empty_key_env_refuses_everything(self):
        os.environ["VLLM_API_KEY"] = ""
        with self.assertRaises(RuntimeError):
            RequireEngineKey(echo_app)

    def test_websocket_without_key_is_closed(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        self.assertFalse(websocket_accepted(client, "/collective_rpc"))

    def test_websocket_with_key_is_forwarded(self):
        os.environ["VLLM_API_KEY"] = "k3y"
        client = make_client(RequireEngineKey(echo_app))

        self.assertTrue(
            websocket_accepted(
                client, "/collective_rpc", headers={"Authorization": "Bearer k3y"}
            )
        )


if __name__ == "__main__":
    unittest.main()
