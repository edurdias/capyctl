# Reaching inference from other machines

CapyCTL's inference endpoint is the OpenAI-compatible API that clients send
requests to. From 0.1.0 it listens on every interface of the machine and
requires an API key, so a laptop, a phone or a Tailscale peer can use the models
on your GPU machine without extra software. This page explains where the key is,
how to narrow who can connect, and how to put the endpoint on the internet
safely.

Everything here applies to `capyctl start standalone` and `capyctl start server`. A
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

Standalone (the state root is `~/.local/state/capyctl` unless `--state-dir` or
`CAPYCTL_STATE_DIR` names another; the packaged system unit uses
`/var/lib/capyctl/standalone`):

```bash
grep '^api_key:' ~/.local/state/capyctl/identity/credentials
```

Server (created by `capyctl init server` in the document's `identity_dir`):

```bash
sudo -u capyctl python3 -c 'import json; print(json.load(open("/var/lib/capyctl/server/identity/server-credentials.json"))["api_key"])'
```

CapyCTL never prints the key. Copy the value to the client machine and keep it
secret. On the client:

```bash
KEY=...   # the api_key value
curl -H "Authorization: Bearer $KEY" http://<host>:8443/v1/models
```

`<host>` is the GPU machine's LAN name or address, or its Tailscale name. A
request without the key, or with a wrong one, is answered `401`.

The address follows the usual settings rule (see the
[settings reference](configuration.md#listeners)): `--listen` for one run, then
`CAPYCTL_INFERENCE_ADDR`, then `listeners.inference.bind` in the document
(`server.listeners.inference.bind` in a standalone document), then
`0.0.0.0:8443`. IPv6 is accepted (`[::]:8443`).

## Narrow to a Tailscale address

Tailscale encrypts traffic between your devices, so plain HTTP is acceptable
there. Binding to the machine's tailnet address keeps the endpoint off the LAN
and every other interface:

```bash
capyctl start standalone --listen "$(tailscale ip -4):8443"
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
capyctl start standalone --listen 127.0.0.1:8443
```

or set `listeners.inference.bind: "127.0.0.1:8443"` in the document. The
`CAPYCTL_INFERENCE_ADDR` variable does the same for a service unit
(`/etc/capyctl/standalone.env` or `server.env`).

## Turning the key off

There are three ways, highest precedence first:

- `--no-inference-auth` for one run;
- `CAPYCTL_INFERENCE_AUTH=none` in the environment (`api_key` turns it back on);
- `listeners.inference.authentication: none` in the document
  (`server.listeners.inference.authentication` in standalone).

When the key is off and the address is not loopback, the role prints this at
start, before it accepts connections:

```
WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key.
Anyone who can reach this address can use your models and GPU.
Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen.
```

and `capyctl status` shows `inference: unauthenticated on 0.0.0.0:8443`. On a
loopback address nothing is printed.

Do not turn the key off beyond loopback. Anyone who can reach the address can
then run any model you have deployed, wake parked models, and use your GPU and
power, and on a shared or public network that is everyone nearby. There is no
fallback key: if the credentials file cannot be read, the role refuses to start.

## Internet exposure through a TLS reverse proxy

CapyCTL does not terminate TLS. To reach the endpoint from the internet, keep CapyCTL
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

## Web UI off the machine

The [web UI](../guide/web-ui.md) and the management API share the management
listener, which stays on loopback (`127.0.0.1:7443`) unless the role allows it
off the machine. One setting does that, in any of these forms:

- `remote: true` on the management listener in the document
  (`listeners.management.remote` on a server,
  `server.listeners.management.remote` in a standalone document);
- `--management-remote` on `capyctl start standalone` or `capyctl start server`;
- `CAPYCTL_MANAGEMENT_REMOTE=true` in the environment (`false` turns it off
  for that run).

Without it, a non-loopback management address is refused at start, as before.
With it, the management address may be any address with a port. Bind the
machine's Tailscale address to keep the UI on your tailnet:

```yaml
# standalone.yaml (a server document has the same block at the top level)
server:
  listeners:
    management:
      bind: "100.64.0.21:7443"   # this machine's `tailscale ip -4`
      remote: true
```

The UI is plain HTTP and CapyCTL prints no warning about it: the network
provides the protection. In the tailnet policy file, allow only your own
devices to reach port 7443, as for [inference](#narrow-to-a-tailscale-address).

From another machine, only a signed-in browser is accepted. The admin token
is accepted only from the machine itself (loopback, or the machine's own
address), and never on a request that carries `X-Forwarded-For` or
`Forwarded`, so a proxy cannot pass it through. CLI commands on the machine
keep working with any bind; on `0.0.0.0` or `::` they connect through
loopback.

### Sign-in links and `public_address`

`capyctl open ui` prints a sign-in link for the address a browser should use:
`--address <origin>` if given, else `public_address`, else the bind address
when it is a specific one. Set `public_address` to an origin (scheme, host
and port, no path) when the browser reaches the UI at another name than the
bind address, for example a LAN name:

```yaml
server:
  listeners:
    management:
      bind: "0.0.0.0:7443"
      remote: true
      public_address: "http://gpu-box:7443"
```

The browser's requests that change something must come from the address they
are sent to, or from `public_address`; others are refused `403
origin_refused`.

### HTTPS with `tailscale serve`

`tailscale serve` gives the UI an HTTPS name on your tailnet with a
certificate, while CapyCTL stays on loopback. No `remote` is needed; name the
HTTPS origin in `public_address`:

```yaml
server:
  listeners:
    management:
      bind: "127.0.0.1:7443"
      public_address: "https://gpu-box.tail1234.ts.net"
```

```bash
tailscale serve --bg 7443
```

Sign-in links then use the HTTPS name. Requests through `tailscale serve`
carry `X-Forwarded-For`, so they are accepted only with a browser session,
never with the admin token.

### When Tailscale starts after CapyCTL

A role whose management `bind` names the Tailscale address fails to start
when that address does not exist yet, as at boot when `tailscaled` comes up
after CapyCTL; the listener cannot bind, like any listener. Either bind
`0.0.0.0:7443` (with `remote: true`) and limit who can reach the port with the
tailnet policy or a firewall, or order the role's service after Tailscale with
a drop-in (`systemctl edit capyctl-standalone`):

```ini
[Unit]
Wants=tailscaled.service
After=tailscaled.service
```

### Risks you accept with `remote`

- **Plain HTTP off the machine.** On a plain LAN, the session cookie travels
  unencrypted, and so do host invitation secrets, prompts and replies in the
  playground, engine logs and configuration. Use Tailscale, or
  `tailscale serve` for HTTPS.
- **Custom engines from any session.** Anyone holding a signed-in browser who
  can reach the address can add a custom engine, which runs any program as the
  CapyCTL user. The tailnet policy or your LAN is the only boundary. On a
  server, that reaches every enrolled host.
- **A session is as strong as the admin token.** Revoking it does not undo
  what it ran.
- **The cookie is sent to every port on the same host name.** Browsers do not
  separate cookies by port, so another web server on the same name and a
  different port receives it.

## What is never exposed

- **Management with the admin token.** The management API (deploy, start,
  stop, status) accepts its admin token only from the machine itself, on every
  role and in every release; the listener stays on loopback unless you
  [allow the web UI off the machine](#web-ui-off-the-machine), and then only a
  signed-in browser is accepted from elsewhere. A server's
  bootstrap and control listeners keep mutual TLS for enrolled hosts.
- **Engines.** Every engine listens on loopback ports only. vLLM and SGLang
  each get a key generated for that launch and checked by CapyCTL's guard;
  TensorFold has no key. The router
  is the only path from the network to an engine, and it forwards only the
  allowlisted inference routes: `POST /v1/chat/completions`, and
  `POST /v1/systemone` for decision models. A model running across machines
  also opens peer ports; see below.

## Multi-node groups

A model running across machines ([One model across machines](../guide/several-machines.md#one-model-across-machines))
keeps its API and control endpoints on loopback, behind the same keys. Its
members also talk to each other on ports that have no authentication:

- the rendezvous port on the head, from `--rendezvous-ports` (default
  `25000-25099`), on every interface;
- the engine's own ports, which CapyCTL does not choose: the gloo ports of the
  CPU process group on each member, vLLM's broadcast queue port, an extra port
  TensorFold opens on rank 0, for SGLang with DP attention `P+1` to `P+6` and
  `P+13` from the rendezvous port `P` plus one kernel-picked port per DP rank,
  and NCCL's dynamic ports.

The engines exchange pickled Python objects over these ports. Anyone who can
reach them can likely run code as the user the engine runs as, and read the
per-launch keys inside that process; at the least they can disturb or crash
the group. This holds on every network the machines are on, including a
wireless or overlay network such as a tailnet.

CapyCTL checks no firewall. Keep group machines on a private direct link, and
block the ports above on every other interface. Status marks every group
`peer transport unauthenticated` (`"peer_transport": "unauthenticated"` in
JSON). Each listener, the address it binds and example firewall rules:
[Ports a group opens](../guide/several-machines.md#ports-a-group-opens).

## Upgrading from an earlier release

CapyCTL never rewrites your configuration. A document an earlier release
generated keeps its `127.0.0.1:8443` inference bind, so inference stays on the
machine until you change it. To serve the network, set the bind to
`0.0.0.0:8443` (or the machine's Tailscale address) or start with
`--listen 0.0.0.0:8443`. Newly generated documents already use `0.0.0.0:8443`.
