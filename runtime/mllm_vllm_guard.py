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

SPEC §9.1 / T21: when mllm also names a per-launch admin key
(`MLLM_VLLM_ADMIN_KEY`), control is keyed apart from inference. The inference
key then reaches only the inference prefixes vLLM itself guards (`/v1`, `/v2`,
`/inference`, `/cohere`, on a normalized path) and `/metrics`; every other
path, the development routes included, takes the admin key alone. The two keys
must differ.
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
_ADMIN_ENV = "MLLM_VLLM_ADMIN_KEY"
# vLLM's own GUARDED_PREFIX set, plus the metrics the host agent scrapes.
_INFERENCE_PREFIXES = ("/v1", "/v2", "/inference", "/cohere")
_INFERENCE_EXACT = ("/metrics",)


def _inference_path(path):
    """A normalized path under an inference prefix, or `/metrics`."""
    if type(path) is not str or not path.startswith("/"):
        return False
    segments = path.split("/")[1:]
    if any(segment in ("", ".", "..") for segment in segments[:-1]) or \
            (segments and segments[-1] in (".", "..")):
        return False
    if path in _INFERENCE_EXACT:
        return True
    return any(path == prefix or path.startswith(prefix + "/")
               for prefix in _INFERENCE_PREFIXES)


class RequireEngineKey:
    """Pure ASGI middleware requiring the engine key on every guarded path."""

    def __init__(self, app):
        key = os.environ.get("VLLM_API_KEY")
        if not key:
            raise RuntimeError(
                "RequireEngineKey requires a non-empty VLLM_API_KEY at construction"
            )
        admin = os.environ.get(_ADMIN_ENV)
        if admin is not None and (not admin or secrets.compare_digest(
                admin.encode("utf-8"), key.encode("utf-8"))):
            raise RuntimeError(
                "RequireEngineKey requires a distinct non-empty admin key when one is named"
            )
        self.app = app
        self._expected_hash = hashlib.sha256(key.encode("utf-8")).digest()
        self._admin_hash = (hashlib.sha256(admin.encode("utf-8")).digest()
                            if admin is not None else None)

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

        if self._is_authorized(scope, self._required_hashes(scope)):
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

    def _required_hashes(self, scope):
        """The key hashes that open this path (SPEC §9.1 / T21)."""
        if self._admin_hash is None:
            return (self._expected_hash,)
        if _inference_path(scope.get("path")):
            return (self._expected_hash,)
        return (self._admin_hash,)

    def _is_authorized(self, scope, accepted):
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
        matched = False
        for expected in accepted:
            matched |= secrets.compare_digest(candidate_hash, expected)
        return matched
