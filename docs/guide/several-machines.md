# Run on several machines

One machine runs the server; each GPU machine runs a host. Clients send every
request to the server, which forwards it to the machine running that model.
Install mllm on every machine first ([Install](install.md)).

The machines reach each other over a private network. A GPU machine needs an
address in `100.64.0.0/10`, as a Tailscale tailnet gives; the server sends
requests to its engines there. Below, the server is `100.64.0.10` and the GPU
machine `gpu-box` is `100.64.0.21`.

A host describes its memory as one pool shared by the GPU and the system, as
on unified-memory machines. A host whose GPU has its own separate memory is
not supported yet; run such a machine on its own with
[standalone](one-machine.md).

## 1. Start the server

On the server machine:

```bash
mllm init server --output server.yaml
```

```text
{"config":"server.yaml","initialized":true,"state_dir":"/home/me/.local/state/mllm"}
```

Edit `server.yaml` so hosts can reach it. Set the `bootstrap` and `control`
listeners to the server's address, and the two `enrollment` addresses to a
name or address hosts use for it. Keep `management` and `inference` on
`127.0.0.1`.

```yaml title="server.yaml"
schema_version: 1
kind: server
name: mllm-server
state_dir: /home/me/.local/state/mllm
identity_dir: /home/me/.local/state/mllm/identity
listeners:
  management:
    bind: "127.0.0.1:7443"
    authentication: token
  inference:
    bind: "127.0.0.1:8443"
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
mllm start server --config ~/server.yaml
```

```text
{"management":"127.0.0.1:7443","role":"server","state_dir":"/home/me/.local/state/mllm"}
```

Leave it running. Pass `--config ~/server.yaml` to every command you run on
the server.

## 2. Invite a GPU machine

On the server:

```bash
mllm invite host gpu-box --output gpu-box.join --config ~/server.yaml
```

```text
{"host_name":"gpu-box","invitation_file":"gpu-box.join"}
```

Copy `gpu-box.join` to the GPU machine, for example with `scp`. It works once.

## 3. Join and start the host

On the GPU machine:

```bash
mllm init host --output host.yaml
```

```text
{"config":"host.yaml","initialized":true,"runtime_dir":"/home/me/.local/state/mllm/runtime","state_dir":"/home/me/.local/state/mllm"}
```

Edit `host.yaml`: its `name` (the one in the invitation), `model_store` (your
models directory), `ingress` (this machine's private address), two labels of
your choice for the hardware and the software, and its memory: how much mllm
may hand out (`managed_limit`) and how much to keep free (`free_reserve`).

```yaml title="host.yaml"
schema_version: 1
kind: host
name: gpu-box
state_dir: /home/me/.local/state/mllm
identity_dir: /home/me/.local/state/mllm/identity
model_store:
  path: /home/me/models
ingress:
  transport: trusted_private_link
  address: "http://100.64.0.21:8444"
  bind: "100.64.0.21:8444"
hardware_fingerprint: gpu-box-1
environment_fingerprint: gpu-box-env-1
runtime_profiles: {}
resource_policy:
  domains:
    unified:
      memory: unified
      managed_limit: "40GiB"
      free_reserve: "8GiB"
  devices:
    gpu0:
      domain: unified
      sharing: shared
  device_sharing: shared
  endpoint_port_range:
    start: 8100
    end: 8199
```

```bash
mllm validate config --file ~/host.yaml
mllm join host --join-file gpu-box.join --config ~/host.yaml
```

```text
{"file":"/home/me/host.yaml","kind":"host","resolved_against":null,"valid":true}
{"enrolled":true,"host_id":"01M3CQBFG2TRXXWTAMAK7X6D1A"}
```

```bash
mllm start host --config ~/host.yaml
```

Leave it running.

## 4. Add engines on each host

In a second terminal on the GPU machine:

```bash
mllm engine add ~/venvs/vllm --config ~/host.yaml
```

```text
{"custom":false,"deep_park":"enabled","deep_park_probe":"available","engine":"vllm","engines_file":"/home/me/engines.yaml","executable":"/home/me/venvs/vllm/bin/vllm","fingerprint":{"digest":"sha256:75e6dea2b0a0bb2a620d8ac4c492c5cb89d9fb0debaa09c3504bfcd6c7adae57","version":"0.29.0"},"profile":"vllm","published":"published","revision":1,"version":"0.29.0"}
```

Repeat steps 2 to 4 for each GPU machine. On the server:

```bash
mllm list hosts --config ~/server.yaml
mllm list engines --config ~/server.yaml
```

```text
NAME      STATE    ELIGIBLE   VERSION      COMPATIBILITY   MEMORY (FREE / TOTAL)   ENGINES
gpu-box   online   yes        0.1.0-rc.4   supported       31.8 GiB / 61.2 GiB     vllm
HOST      PROFILE   ENGINE   VERSION   CUSTOM   DEEP PARK   STATE    DEPLOYMENTS
gpu-box   vllm      vllm     0.29.0    no       enabled     online   -
```

## 5. Deploy

Use the file from [Deploy a model](deploy.md) with one more line,
`host: gpu-box`, after `name`. On the server:

```bash
mllm deploy model --file my-model.yaml --config ~/server.yaml
mllm start deployment my-model --wait --config ~/server.yaml
mllm list deployments --config ~/server.yaml
```

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     1          gpu-box
```

## 6. Send a request

On the server, with the server's API key:

```bash
KEY=$(sed -n 's/.*"api_key": *"\([^"]*\)".*/\1/p' ~/.local/state/mllm/identity/server-credentials.json)
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}'
```

```text
{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"Hello! How can I help you today?","role":"assistant"}}],"created":1790354782,"id":"chatcmpl-1","model":"my-model","object":"chat.completion"}
```

To send requests from other computers, see
[Make a request](requests.md#from-another-machine).

## Take a machine out

`mllm drain host gpu-box --config ~/server.yaml` stops its models; they start
again when a request needs them. `mllm revoke host gpu-box --config ~/server.yaml`
disconnects it for good. To bring it back:

```bash
mllm invite host gpu-box --recover --output gpu-box.join --config ~/server.yaml   # on the server
mllm join host --join-file gpu-box.join --recover --config ~/host.yaml            # on gpu-box
mllm start host --config ~/host.yaml
```
