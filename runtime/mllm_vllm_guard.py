"""ASGI middleware that closes vLLM's unguarded development routes.

vLLM 0.29 authenticates only the `/v1`, `/v2`, `/inference`, and `/cohere`
prefixes (`vllm/entrypoints/serve/middleware/authenticate.py`,
`GUARDED_PREFIX`); the parking control routes mllm depends on for deep-park
(`/sleep`, `/wake_up`, `/is_sleeping`, `/collective_rpc`) are left open to any
local caller in development mode. SPEC §9.1 requires that surface to stay
denied by default, with explicit opt-in only (SPEC §9.1 / T21: "Default
denial, explicit opt-in required; no public admin passthrough.").

mllm loads this middleware into the vLLM server process itself via vLLM's
own `--middleware mllm_vllm_guard.RequireEngineKey` flag, so it must import
cleanly with nothing beyond the standard library: vLLM's venv has no mllm
package or third-party extras installed for it.

`RequireEngineKey` requires a valid `Authorization: Bearer <VLLM_API_KEY>`
header on every HTTP and WebSocket path except `/health`; `OPTIONS` requests
pass through unchecked (SPEC §9.1: preflight is not a control action).
Comparison mirrors vLLM's own key check
(`vllm/entrypoints/serve/middleware/authenticate.py`): the presented token is
hashed with SHA-256 and compared to the configured key's hash using
`secrets.compare_digest`, so unequal-length or plaintext-timing differences
never leak information about the configured key.
"""

import hashlib
import json
import os
import secrets

_UNAUTHORIZED_BODY = json.dumps({"error": "Unauthorized"}).encode("utf-8")
_HEALTH_PATH = "/health"


class RequireEngineKey:
    """Pure ASGI middleware requiring the engine key on every guarded path."""

    def __init__(self, app):
        key = os.environ.get("VLLM_API_KEY")
        if not key:
            raise RuntimeError(
                "RequireEngineKey requires a non-empty VLLM_API_KEY at construction"
            )
        self.app = app
        self._expected_hash = hashlib.sha256(key.encode("utf-8")).digest()

    async def __call__(self, scope, receive, send):
        if scope["type"] not in ("http", "websocket"):
            # Only HTTP and WebSocket carry the routes SPEC §9.1 gates;
            # lifespan and other scope types pass straight through.
            await self.app(scope, receive, send)
            return

        if scope.get("path") == _HEALTH_PATH:
            await self.app(scope, receive, send)
            return

        if scope["type"] == "http" and scope.get("method", "").upper() == "OPTIONS":
            await self.app(scope, receive, send)
            return

        if self._is_authorized(scope):
            await self.app(scope, receive, send)
            return

        if scope["type"] == "websocket":
            # Consume the connect event before closing; sending a close
            # without first receiving is invalid per the ASGI spec.
            await receive()
            await send({"type": "websocket.close", "code": 4401})
            return

        await send(
            {
                "type": "http.response.start",
                "status": 401,
                "headers": [(b"content-type", b"application/json")],
            }
        )
        await send({"type": "http.response.body", "body": _UNAUTHORIZED_BODY})

    def _is_authorized(self, scope):
        header_value = None
        for name, value in scope.get("headers") or ():
            if name.lower() == b"authorization":
                header_value = value
                break
        if not header_value:
            return False
        try:
            text = header_value.decode("latin-1")
        except UnicodeDecodeError:
            return False
        scheme, _, token = text.partition(" ")
        if scheme.lower() != "bearer" or not token:
            return False
        candidate_hash = hashlib.sha256(token.encode("utf-8")).digest()
        return secrets.compare_digest(candidate_hash, self._expected_hash)
