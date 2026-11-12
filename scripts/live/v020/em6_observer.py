"""EM6-only hook in a fresh vLLM environment; never inspect header values."""
import itertools
import json
import os
from pathlib import Path
import time

# setup.sh substitutes the session-private destination before installation.
OUTPUT = Path('__EM6_OUTPUT__')
_IDS = itertools.count()


def record(data):
    data['monotonic_ns'] = time.monotonic_ns()
    descriptor = os.open(OUTPUT, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        os.write(descriptor, (json.dumps(data, separators=(',', ':'))+'\n').encode())
    finally:
        os.close(descriptor)


class Observe:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope.get('type') != 'http' or scope.get('path') != '/v1/embeddings':
            return await self.app(scope, receive, send)
        request = str(os.getpid())+'-'+str(next(_IDS))
        # Deliberately bind only tuple element zero: header values, including
        # the per-launch engine key, are never read, decoded or exported.
        names = sorted({pair[0].decode('ascii').lower() for pair in scope.get('headers', ())})
        record(dict(request=request, event='request_headers', header_names=names))
        request_bytes = response_bytes = 0
        async def observed_receive():
            nonlocal request_bytes
            message = await receive()
            if message['type'] == 'http.request':
                request_bytes += len(message.get('body', b''))
            return message
        async def observed_send(message):
            nonlocal response_bytes
            if message['type'] == 'http.response.start':
                record(dict(request=request, event='response_start', status=message['status']))
            if message['type'] == 'http.response.body':
                body = message.get('body', b'')
                # Preserve bytes, status and finality, fragmenting only the
                # transport into bounded writes so relay/backpressure is observable.
                for offset in range(0, max(1, len(body)), 65536):
                    chunk = body[offset:offset+65536]
                    final = offset+65536 >= len(body) and not message.get('more_body', False)
                    await send(dict(message, body=chunk, more_body=not final))
                    response_bytes += len(chunk)
                    record(dict(request=request, event='response_body', bytes=len(chunk),
                                total_bytes=response_bytes, final=final))
                return
            await send(message)
        try:
            await self.app(scope, observed_receive, observed_send)
        finally:
            record(dict(request=request, event='complete', request_bytes=request_bytes, response_bytes=response_bytes))


def install():
    from uvicorn.config import Config
    original = Config.load
    if getattr(original, '_capyctl_em6', False):
        return
    def load(config):
        original(config)
        config.loaded_app = Observe(config.loaded_app)
    load._capyctl_em6 = True
    Config.load = load
