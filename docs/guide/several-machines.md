# Run on several machines

One machine runs the server; each GPU machine runs a host. Clients send every
request to the server, which forwards it to the machine running that model.
Install CapyCTL on every machine first ([Install](install.md)).

The machines reach each other over a private network. A GPU machine needs an
address in `100.64.0.0/10`, as a Tailscale tailnet gives; the server sends
requests to its engines there. Below, the server is `100.64.0.10` and the GPU
machine `gpu-box` is `100.64.0.21`.

## 1. Start the server

On the server machine:

```bash
capyctl init server --output server.yaml
```

```text
Wrote server.yaml

  State directory   /home/me/.local/state/capyctl
```

Edit `server.yaml` so hosts can reach it. Set the `bootstrap` and `control`
listeners to the server's address, and the two `enrollment` addresses to a
name or address hosts use for it. Keep `management` on `127.0.0.1`. The
`inference` listener is the endpoint your apps use; it serves every interface
and needs the API key.

```yaml title="server.yaml"
schema_version: 1
kind: server
name: capyctl-server
state_dir: /home/me/.local/state/capyctl
identity_dir: /home/me/.local/state/capyctl/identity
listeners:
  management:
    bind: "127.0.0.1:7443"
    authentication: token
  inference:
    bind: "0.0.0.0:8443"
    authentication: api_key
  bootstrap:
    bind: "100.64.0.10:7444"
    authentication: server_tls
  control:
    bind: "100.64.0.10:7445"
    authentication: mutual_tls
enrollment:
  bootstrap_address: "https://100.64.0.10:7444"
  control_address: "https://100.64.0.10:7445"
```

```bash
capyctl start server --config ~/server.yaml
```

```text
capyctl 0.1.2 server ready

  Inference     0.0.0.0:8443 (API key required)
  Management    127.0.0.1:7443
  Bootstrap     100.64.0.10:7444
  Control       100.64.0.10:7445
  State         /home/me/.local/state/capyctl
  Credentials   /home/me/.local/state/capyctl/identity/server-credentials.json
```

Leave it running. The `--config` here is the only one the server needs: the
server records which file it was started with, and every other `capyctl` command
you run on this machine uses it.

## 2. Invite a GPU machine

On the server:

```bash
capyctl invite host gpu-box --output gpu-box.join
```

```text
Invitation for gpu-box written to gpu-box.join

Keep it private; it can be used once.
```

Copy `gpu-box.join` to the GPU machine, for example with `scp -p` to preserve
its permissions. It contains a one-use enrollment secret; on the GPU machine,
ensure only your user can read or write it:

```bash
chmod 600 gpu-box.join
```

## 3. Join and start the host

On the GPU machine:

```bash
capyctl init host --output host.yaml
```

```text
Wrote host.yaml

  State directory     /home/me/.local/state/capyctl
  Runtime directory   /home/me/.local/state/capyctl/runtime
```

CapyCTL sets the memory limits in the file from this machine's memory and GPUs,
and keeps models in `~/models`, so it validates as written. Change two things in
it: set `name` to the one in the invitation, and add an `ingress` with this
machine's private address, where the server sends requests:

```yaml title="host.yaml (the two changes)"
name: gpu-box
ingress:
  transport: trusted_private_link
  address: "http://100.64.0.21:8444"
  bind: "100.64.0.21:8444"
```

To change the memory limits or the models directory, see
[Configuration files](configuration.md).

```bash
capyctl validate config --file ~/host.yaml
capyctl join host --join-file gpu-box.join --config ~/host.yaml
```

```text
/home/me/host.yaml is a valid host document
Joined the server

  Host ID   01M3R7FJRKN5AQMC4EMYSG7KSK
```

```bash
capyctl start host --config ~/host.yaml
```

```text
capyctl 0.1.2 host ready

  Engines       none: run `capyctl engine add <path>`
  Ingress       100.64.0.21:8444
  Host ID       01M3R7FJRKN5AQMC4EMYSG7KSK
  State         /home/me/.local/state/capyctl
  Credentials   /home/me/.local/state/capyctl/identity/host-identity.json
```

The host prints `host ready` once it is up. Leave it
running. The host checks each card in its file against the GPUs it finds, and
refuses to start if they do not match.

## 4. Add engines on each host

In a second terminal on the GPU machine:

```bash
capyctl engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.29.0)

  Executable     /home/me/venvs/vllm/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/engines.yaml (revision 1)
  Published      yes
```

The host records the file it was started with, so `capyctl engine add` on this
machine finds `host.yaml` without `--config`, and keeps the engine list beside
it, in `engines.yaml`.

A host has no management API. A command that needs the server, run on the GPU
machine, says so:

```text
$ capyctl list deployments
error [invalid_config]: This machine is a capyctl host; run this command on the server. A host has no management API: deployments, hosts and invitations are managed where the server (or a standalone role) runs
```

Repeat steps 2 to 4 for each GPU machine. On the server:

```bash
capyctl list hosts
capyctl list engines
```

```text
NAME      STATE    ELIGIBLE   VERSION   COMPATIBILITY   MEMORY (FREE / TOTAL)   ENGINES
gpu-box   online   yes        0.1.2     supported       46.8 GiB / 77.2 GiB     vllm
HOST      PROFILE   ENGINE   VERSION   CUSTOM   DEEP PARK   STATE    DEPLOYMENTS
gpu-box   vllm      vllm     0.29.0    no       enabled     online   -
```

## 5. Deploy

Use the file from [Deploy a model](deploy.md). CapyCTL places the model on a
machine with room; add `host: gpu-box` to choose one. On the server:

```bash
capyctl deploy model --file my-model.yaml --activate --wait
```

```text
Request identity: 01M3R7GSP8ZQY0YGNTHDVJBSGN (reuse --request-id 01M3R7GSP8ZQY0YGNTHDVJBSGN to recover this command)
Waiting for the checkpoint digest of my-model to be measured (at most 900s)
Deployed my-model: ready

  Revision    1
  Hosts       gpu-box
  Ready       1/1
  Startup     28.5 GiB
  Operation   initialize succeeded
```

```bash
capyctl list deployments
```

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

## 6. Send a request

On the server, with the server's API key:

```bash
KEY=$(sed -n 's/.*"api_key": *"\([^"]*\)".*/\1/p' ~/.local/state/capyctl/identity/server-credentials.json)
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790466141,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

From other computers, use the server's address instead of `127.0.0.1`
([Make a request](requests.md#from-another-machine)).

## Take a machine out

On the server, `capyctl drain host gpu-box` stops its models; they start again
when a request needs them. `capyctl revoke host gpu-box` disconnects it for good.
To bring it back:

```bash
capyctl invite host gpu-box --recover --output gpu-box.join              # on the server
capyctl join host --join-file gpu-box.join --recover --config ~/host.yaml   # on gpu-box
capyctl start host --config ~/host.yaml
```
