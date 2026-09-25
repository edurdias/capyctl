# Several machines

One machine runs the server; each GPU machine runs a host. Clients send every
request to the server, which forwards it to the machine running that model.
Install mllm on every machine first ([Install](install.md)).

## 1. Start the server

```bash
mllm init server --output server.yaml
# edit server.yaml: the addresses hosts and clients use to reach it
mllm start server --config server.yaml
```

## 2. Add a GPU machine

On the server, create an invitation for a host named `gpu-box`:

```bash
mllm invite host gpu-box --output gpu-box.join --config server.yaml
```

Copy `gpu-box.join` to the GPU machine. There:

```bash
mllm init host --output host.yaml
# edit host.yaml: its name, model directory and address
mllm join host --join-file gpu-box.join --config host.yaml
mllm start host --config host.yaml
```

In another terminal on the GPU machine, register your engine. It is
published as the runtime profile `vllm` (or `sglang`):

```bash
mllm engine add ~/venvs/vllm --config host.yaml
```

Repeat for each GPU machine. `mllm list hosts --config server.yaml` shows them.

## 3. Deploy

Write a deployment file like the [one-host example](configuration.md#deployment-on-one-host):
`host` names the machine and `runtime_profile` the engine you added.

```bash
mllm deploy model --file deployment.yaml --activate --wait --config server.yaml
mllm list deployments --config server.yaml
```

Clients use `https://<server>:8443/v1`.

## Take a machine out

`mllm drain host gpu-box --config server.yaml` stops its models; they start
again on demand. `mllm revoke host gpu-box --config server.yaml` disconnects
it for good; its engines keep running but get no requests. To bring it back:

```bash
mllm invite host gpu-box --recover --output gpu-box.join --config server.yaml   # on the server
mllm join host --join-file gpu-box.join --recover --config host.yaml            # on gpu-box
mllm start host --config host.yaml
```
