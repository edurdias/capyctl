# Reaching inference from other machines

mllm's inference endpoint is the OpenAI-compatible API that clients send
requests to. From 0.1.0 it listens on every interface of the machine and
requires an API key, so a laptop, a phone or a Tailscale peer can use the models
on your GPU machine without extra software. This page explains where the key is,
how to narrow who can connect, and how to put the endpoint on the internet
safely.

Everything here applies to `mllm start standalone` and `mllm start server`. A
host role has no inference listener of its own; the server forwards to it over
its private ingress.

## Default

| | |
|---|---|
| Address | `0.0.0.0:8443` (all interfaces, port 8443) |
| Protocol | plain HTTP |
| Authentication | `Authorization: Bearer <api key>` required on every request |
| The key | generated on first start: a random 256-bit value, never printed or logged |

The key lives in an owner-only file under the role's state directory.

Standalone (the state root is `~/.local/state/mllm` unless `--state-dir` or
`MLLM_STATE_DIR` names another; the packaged system unit uses
`/var/lib/mllm/standalone`):

```bash
grep '^api_key:' ~/.local/state/mllm/identity/credentials
```

Server (created by `mllm init server` in the document's `identity_dir`):

```bash
sudo -u mllm python3 -c 'import json; print(json.load(open("/var/lib/mllm/server/identity/server-credentials.json"))["api_key"])'
```

mllm never prints the key. Copy the value to the client machine and keep it
secret. On the client:

```bash
KEY=...   # the api_key value
curl -H "Authorization: Bearer $KEY" http://<host>:8443/v1/models
```

`<host>` is the GPU machine's LAN name or address, or its Tailscale name. A
request without the key, or with a wrong one, is answered `401`.

The address follows the usual settings rule (see the
[settings reference](configuration.md#listeners)): `--listen` for one run, then
`MLLM_INFERENCE_ADDR`, then `listeners.inference.bind` in the document
(`server.listeners.inference.bind` in a standalone document), then
`0.0.0.0:8443`. IPv6 is accepted (`[::]:8443`).

## Narrow to a Tailscale address

Tailscale encrypts traffic between your devices, so plain HTTP is acceptable
there. Binding to the machine's tailnet address keeps the endpoint off the LAN
and every other interface:

```bash
mllm start standalone --listen "$(tailscale ip -4):8443"
```

To keep it, state the same address in the document instead:

```yaml
# standalone.yaml (a server document has the same block at the top level)
server:
  listeners:
    inference:
      bind: "100.64.0.21:8443"   # this machine's `tailscale ip -4`
      authentication: api_key
```

In the tailnet policy file, allow only your own devices to reach port 8443 on
the GPU machine, for example:

```json
{
  "tagOwners": {"tag:gpu": ["autogroup:admin"]},
  "acls": [
    {"action": "accept", "src": ["autogroup:member"], "dst": ["tag:gpu:8443"]}
  ]
}
```

Tag the GPU machine `tag:gpu`. Keep the API key on: the tailnet decides who can
connect, the key decides who can use the models.

## Loopback only

To serve only programs on the GPU machine itself, as releases before 0.1.0 did:

```bash
mllm start standalone --listen 127.0.0.1:8443
```

or set `listeners.inference.bind: "127.0.0.1:8443"` in the document. The
`MLLM_INFERENCE_ADDR` variable does the same for a service unit
(`/etc/mllm/standalone.env` or `server.env`).

## Turning the key off

There are three ways, highest precedence first:

- `--no-inference-auth` for one run;
- `MLLM_INFERENCE_AUTH=none` in the environment (`api_key` turns it back on);
- `listeners.inference.authentication: none` in the document
  (`server.listeners.inference.authentication` in standalone).

When the key is off and the address is not loopback, the role prints this at
start, before it accepts connections:

```
WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key.
Anyone who can reach this address can use your models and GPU.
Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen.
```

and `mllm status` shows `inference: unauthenticated on 0.0.0.0:8443`. On a
loopback address nothing is printed.

Do not turn the key off beyond loopback. Anyone who can reach the address can
then run any model you have deployed, wake parked models, and use your GPU and
power, and on a shared or public network that is everyone nearby. There is no
fallback key: if the credentials file cannot be read, the role refuses to start.

## Internet exposure through a TLS reverse proxy

mllm does not terminate TLS. To reach the endpoint from the internet, keep mllm
on `127.0.0.1:8443` (or on the tailnet address, with the proxy on another
tailnet machine) and put a TLS reverse proxy in front of it. With Caddy, which
obtains and renews the certificate itself:

```
models.example.com {
    reverse_proxy 127.0.0.1:8443 {
        flush_interval -1
    }
}
```

`flush_interval -1` keeps streaming responses streaming instead of buffering
them. Keep the API key on: the proxy provides encryption, not authentication.
Open only port 443 to the internet, never 8443.

## What is never exposed

- **Management.** The management API (deploy, start, stop, status) stays on
  loopback with its own admin token, on every role and in every release. Manage
  a machine remotely with `ssh` and the local `mllm` commands. A server's
  bootstrap and control listeners keep mutual TLS for enrolled hosts.
- **Engines.** vLLM and SGLang listen on loopback ports only, each with a key
  generated for that launch and checked by mllm's guard. The router
  is the only path from the network to an engine, and it forwards only the
  allowlisted inference routes.

## Upgrading from an earlier release

A server or standalone document whose inference bind is exactly
`127.0.0.1:8443` (the old generated default) is changed once, at the first start
of 0.1.0, to `0.0.0.0:8443`. A copy of the old file is kept as
`<file>.pre-0.1.0`, and the start prints:

```
NOTICE: mllm 0.1.0 serves inference on all interfaces: 0.0.0.0:8443 (was 127.0.0.1:8443).
The API key is still required. Configuration updated: <path> (previous copy: <path>.pre-0.1.0).
To keep inference local, start with --listen 127.0.0.1:8443 or set listeners.inference.bind.
```

It happens once: the marker `<state dir>/migrations/inference-bind-v1` records
it, so setting `127.0.0.1:8443` back afterwards is kept. Any other address,
and the authentication setting, are never changed. If the document cannot be
rewritten (read-only, or the address appears more than once in it), it is left
as it is, the role serves on `0.0.0.0:8443` for that run only, and the notice
says which line to edit; later starts follow the document again.
