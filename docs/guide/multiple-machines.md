# Multiple machines

One machine runs the server. Each GPU machine runs a host. The examples use
`gpu-box` as the host name; install mllm on every machine first
([Install](../operations/install.md)).

## Server

```bash
mllm init server --output server.yaml
# edit server.yaml: listeners and enrollment addresses
mllm validate config --file server.yaml
mllm start server --config server.yaml
```

See the [server document](configuration.md#server).

## Invite and join a host

On the server, create a single-use invitation:

```bash
mllm invite host gpu-box --output gpu-box.join --config server.yaml
```

Copy `gpu-box.join` to the GPU machine, then on it:

```bash
mllm init host --output host.yaml
# edit host.yaml: name, model store, memory policy and engine installations
mllm validate config --file host.yaml
mllm join host --join-file gpu-box.join --config host.yaml
mllm start host --config host.yaml
```

See the [host document](configuration.md#host). To register an engine
installation without editing `host.yaml`, run `mllm engine add <path>` on the
host; see [Registering engines](../operations/install.md#registering-engines).

## Deploy through the server

```bash
mllm list hosts --config server.yaml
mllm deploy model --file deployment.yaml --activate --wait --config server.yaml
mllm list deployments --config server.yaml
```

Clients send every request to the server's endpoint, `https://<server>:8443/v1`.
The server forwards each one to the host running that model.

## Drain a host

Stops the host's engines and keeps its deployments, which start elsewhere or
here again on demand:

```bash
mllm drain host gpu-box --config server.yaml
```

## Revoke and recover

Revoking a host closes its connection for good. Its engines keep running but
receive no requests, and the host role exits with code 14:

```bash
mllm revoke host gpu-box --config server.yaml
```

To bring the same host back under its same identity, create a recovery
invitation on the server and redeem it on the host:

```bash
mllm invite host gpu-box --recover --output gpu-box.join --config server.yaml
mllm join host --join-file gpu-box.join --recover --config host.yaml
mllm start host --config host.yaml
```

The host reconnects and mllm checks each running engine again instead of
restarting it.

## Upgrades

Upgrade the server first, then the hosts one at a time. See
[Install](../operations/install.md#upgrade).
