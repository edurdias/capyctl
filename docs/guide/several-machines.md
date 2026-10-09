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

### A model split across machines

A deployment with a `topology` (for example `tensor_parallel: 2` over
`placement.hosts: [gpu-a, gpu-b]`) runs one model on several machines. Its
memory is per machine. When you state `resources`, each machine reserves them
as written. When CapyCTL derives the memory from the weights, each machine
reserves its own share, not the whole checkpoint: the weights divided by
`tensor_parallel x pipeline_parallel`, plus the tensors every machine keeps
whole (embeddings, output head, norms and routers, read from the checkpoint's
safetensors headers; a tenth of the weights when they cannot be read), plus
the KV cache and margin you would get on one machine. A 126 GiB checkpoint at
`tensor_parallel: 2` reserves about 64 GiB of weights on each machine.
CapyCTL's placeholder for the startup peak is 2.25 times that share, so on two
128 GB machines state `memory.startup` (for example `90GiB`) for such a model.

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

## One model across machines

A model too large for one machine can run as a group: one engine process per
machine, each holding part of the model (one tensor-parallel or
pipeline-parallel rank). The first machine in `placement.hosts` is the head:
it runs rank 0 and serves the API. The others are workers. vLLM, SGLang and
TensorFold can run groups ([Engine support for groups](engines.md#groups-across-machines)).

Below, host A (`192.0.2.10`) is the head and host B (`192.0.2.11`) the
worker, joined by a direct link.

### Before the first group

On each machine, as root (CapyCTL reads these settings and never changes them):

- `vm.compaction_proactiveness` set to 0 (`sysctl vm.compaction_proactiveness=0`).
- Unlimited locked memory for the user CapyCTL runs as (the installed service
  units set `LimitMEMLOCK=infinity`).
- `/dev/infiniband/uverbs*` readable and writable by that user, if the link
  supports RDMA.

A gap is a `host_tuning_warning:<item>` in status and the group still runs.
The compaction check never refuses. With `require_rdma: true`, a missing
`memlock` or `infiniband` refuses the group (`host_tuning_missing:<item>`).

Then, on each machine:

- Declare its address on the direct link in the host document
  (`resource_policy.groups.peer_address`, or `--peer-address`), and restart the
  host. See [`host.yaml`](../examples/host.yaml) and
  [`host-b.yaml`](../examples/host-b.yaml).
- Register the same engine build under the same profile name
  (`capyctl engine add`).
- Keep the same model files on every machine. A SGLang group with
  `residency: deep` also needs them at the same path.

### Deploy a group

[`deployment-multinode.yaml`](../examples/deployment-multinode.yaml) runs a
model at `tensor_parallel: 2` on two machines:

```yaml
topology:
  tensor_parallel: 2
  pipeline_parallel: 1
placement:
  hosts: ["gpu-box", "host-b"]
```

Memory (`engine_config.memory` or `resources`) is per member: each machine
charges its own rank.

Check it first against the host document of every machine it names, with
`--host` repeated once per host in `placement.hosts` (in any order; each is
matched by its `name`):

```bash
capyctl validate config --file docs/examples/deployment-multinode.yaml \
  --host docs/examples/host.yaml --host docs/examples/host-b.yaml
```

It runs the checks deploy runs on each host: the peer address, the profile and
its build, the group shape the engine supports, and the engine variables each
host approves. A refusal carries the same code as deploy's (for example
`peer_address_missing` or `group_profile_mismatch`), and a named host without a
document, or a document for a host the group does not name, is refused with
the host named. It cannot check that the model files sit at the path an engine
needs on each machine (`group_model_path_mismatch`): that is known only once
every machine has the model, so the activation checks it.

Then, on the server:

```bash
capyctl deploy model --file deployment-multinode.yaml --activate --wait
```

Requests go to the server as for any model; the server forwards them to the
head.

### Status

`capyctl status deployment <name>` lists each member. For a group on `host-a`
and `host-b`:

```text
NAME            STATE   READY   REVISION   STARTUP    INITIALIZE   LAST OPERATION
qwen3-30b-tp2   ready   1/1     1          40.0 GiB   1800s        start succeeded

INSTANCE   HOST            STATE   LIFECYCLE   DEVICES   LAST ERROR
0          host-a,host-b   ready   active      -         -

Group of instance 0: vllm, TP 2 x PP 1 on 2 hosts
  rendezvous     192.0.2.10:25000
  peer transport unauthenticated (keep group hosts on a private link)

  RANK  HOST    ROLE    STATE     PROCESSES  CHARGED         RESIDENCY  LAST ERROR
  0     host-a  head    launched  2          40.0 GiB ready  deep       -
  1     host-b  worker  launched  1          40.0 GiB ready  deep       -

  warnings  host-b: host_tuning_warning:compaction
```

### Park, wake, stop and failures

- **Park and wake.** A group with `residency: deep` (vLLM, SGLang) parks on
  every machine at once and wakes the same way. After a wake, CapyCTL checks
  that the model answers a fixed prompt as before; if not, it stops the group
  (`group_wake_mismatch`). TensorFold groups, and groups with
  `residency: restart_only`, park by stopping and wake by starting again.
- **Stop.** Stopping the deployment stops every member.
- **A member fails.** If any member exits or fails, CapyCTL stops the whole
  group (`group_member_failed`). A request with no first token within
  `groups.stall_timeout` whose check through the head also fails stops the
  group too (`group_stalled`).
- **A machine cannot be reached.** Its member stays `uncertain` and keeps its
  memory charged until that machine reconnects and proves the process gone (or
  is revoked and recovered). Status shows which rank holds memory and why, in a
  `held` line. The group does not start again until every member has settled.

### Risk

Group members talk to each other over unauthenticated ports. Anyone who can
reach them can likely run code on those machines. Keep group machines on a
private direct link ([Network access](../operations/network-access.md#multi-node-groups)).

### Ports a group opens

CapyCTL trusts the network between group machines: it
checks no firewall, and it passes each engine the member's address on the
direct link wherever the engine takes one. The engines open the listeners
below. **None of them is authenticated.** The rendezvous store, vLLM's
broadcast queues, SGLang's DP-attention sockets and gloo's object
collectives carry pickled Python objects, and unpickling data from the network
runs code: anyone who can connect can likely run code as the engine's user and
read its per-launch keys.

`P` is the group's rendezvous port, from the head's `--rendezvous-ports`
range (default `25000-25099`). "Ephemeral" means a port the kernel picks from
`net.ipv4.ip_local_port_range` (Ubuntu default `32768-60999`). "Link address"
is the member's `peer_address`. Entries marked *inferred* come from compiled
code (torch, NCCL) or from a different engine version than the one supported,
and are confirmed only by the live check (`ss -ltnp` on both machines).

#### All three engines

| Listener | Where | Port | Binds | Notes |
|---|---|---|---|---|
| Engine API | head | `endpoint_port_range` | `127.0.0.1` | CapyCTL renders `--host 127.0.0.1`; per-launch key; reached only through host ingress and the server. Not reachable from the link. |
| Rendezvous (torch TCP store) | head | `P` | every interface, IPv4 and IPv6 | CapyCTL renders `P` and the head's link address (vLLM `--master-addr/--master-port`, SGLang `--dist-init-addr`, TensorFold `--master/--master-port`). The address tells the other ranks where to connect; the store itself listens on all interfaces (*inferred*: torch c10d is compiled; CapyCTL's `Prepare` probes `P` on `0.0.0.0` and `::` for this reason). |
| NCCL bootstrap and socket transport | every member | ephemeral | an interface NCCL chooses | CapyCTL sets no `NCCL_*` (decision 4). Without `NCCL_SOCKET_IFNAME`, NCCL picks the interface itself (InfiniBand-named first, otherwise the first that is not loopback or a container bridge), which need not be the direct link (*inferred* from NCCL's documented behavior). Over RDMA, data moves over queue pairs, not TCP listeners, and `ss` does not show it. |

#### vLLM 0.30

| Listener | Where | Port | Binds | Source |
|---|---|---|---|---|
| gloo CPU process groups | every member | ephemeral | the link address, through `GLOO_SOCKET_IFNAME` | CapyCTL renders `GLOO_SOCKET_IFNAME` as the interface holding the peer address. |
| Scheduler broadcast queue (ZeroMQ, "shm broadcast" over the network) | head | ephemeral | `VLLM_HOST_IP`, the link address | CapyCTL renders `VLLM_HOST_IP=<peer address>`. vLLM binds `tcp://<VLLM_HOST_IP>:0` for remote readers (`v1/executor/multiproc_executor.py`, `distributed/device_communicators/shm_broadcast.py` in 0.29). |
| Response queues back to rank 0 (ZeroMQ) | every worker | ephemeral | `VLLM_HOST_IP`, the link address | One queue per rank with remote readers, same code path. |

A headless worker opens no API port.

#### SGLang 0.5.21

| Listener | Where | Port | Binds | Source |
|---|---|---|---|---|
| gloo CPU process groups | every member | ephemeral | the link address, through `gloo_socket_ifname` | CapyCTL renders it from the peer address. |
| Broadcast queue to remote readers (ZeroMQ) | the writing rank | ephemeral | `SGLANG_HOST_IP`, the link address | CapyCTL renders `SGLANG_HOST_IP=<peer address>`; without it SGLang guesses the default-route address (`srt/utils/network.py`, `shm_broadcast.py` in 0.5.20). |
| Worker health server | every worker | worker loopback port | `127.0.0.1` | CapyCTL renders `--host 127.0.0.1 --port <worker port>`. |
| `nccl_port` | every member | ephemeral | not bound | Chosen but unused: with `--dist-init-addr` the store is on `P`. |

With DP attention enabled, the head also binds, on the head's link address
(the host in `--dist-init-addr`), as SGLang 0.5.21 computes them in
`PortArgs.init_new` and `data_parallel_controller.py`:

| Listener | Port | Binds |
|---|---|---|
| Tokenizer, detokenizer, RPC, metrics, scheduler input, load collector (ZeroMQ) | `P+1` to `P+6` | the head's link address |
| Worker-port handshake (ZeroMQ REP; replies with a pickled port list) | `P+13` | the head's link address |
| One scheduler input socket per DP rank (ZeroMQ PUSH) | ephemeral | the head's link address |

Before it starts the head, CapyCTL checks that `P+1` to `P+6` and `P+13`
are free on the head's link address. SGLang moves the first six down to
`P-7` to `P-2` when `P+6` would pass 65535, but it never moves `P+13`, so a
head whose rendezvous port is above 65522 cannot start with DP attention, and
CapyCTL refuses it. Keep `--rendezvous-ports` at or below 65522 if you use
DP attention. The per-rank sockets take a port the kernel picks as the engine
starts, so nothing can check them beforehand; a conflict there shows only as
an engine start failure.

#### TensorFold 0.6.5

| Listener | Where | Port | Binds | Source |
|---|---|---|---|---|
| Rank exchange (torch TCP store) | head | `P` | every interface (*inferred*, as above) | CapyCTL renders `--master <head> --master-port P`. TensorFold puts NCCL's unique id and readiness keys in this store (`cuda/comm.py` in 0.6.3). |
| NCCL | both | ephemeral | an interface NCCL chooses | TensorFold drives NCCL directly and opens no gloo group. |
| Extra port under `--parallel` | head | ephemeral | not established | Recorded in the design notes for 0.6.5 Flash Next; 0.6.3 refuses `--parallel` with `--tp 2`, so this was not read from source (*inferred*). |

Rank 1 opens no API port.

#### Firewall on the direct link

Apply on every group machine:

1. On the direct-link interface, allow new TCP connections to `P`'s range,
   `P+13`, and the ephemeral range **only from the other machines of the
   group**.
2. On every other interface (LAN, Wi-Fi, a tailnet, IPv6), drop new TCP
   connections to those ports.
3. Leave loopback, SSH, and CapyCTL's own ports (host ingress, the server's
   listeners) as they are; they are outside these ranges unless you moved them
   there.

The ephemeral range covers every engine-chosen port above; check yours with
`sysctl net.ipv4.ip_local_port_range`. Any other service on the machine that
accepts connections on an ephemeral port is blocked from other networks too.

**Example only.** nftables on host A (`192.0.2.10`), with host B
(`192.0.2.11`) on the direct link `enp1s0f0np0`, rendezvous range
`25000-25099` (so `P+13` reaches `25112`). Adjust the addresses, the interface
and the ranges to yours; on host B, list host A:

```text title="/etc/nftables.d/capyctl-group.nft (example)"
table inet capyctl_group {
  set group_peers {
    type ipv4_addr
    elements = { 192.0.2.11 }
  }
  chain input {
    type filter hook input priority filter - 1; policy accept;
    iifname "lo" accept
    iifname "enp1s0f0np0" ip saddr @group_peers tcp dport { 25000-25112, 32768-60999 } accept
    tcp dport { 25000-25112, 32768-60999 } ct state new drop
  }
}
```

**Example only.** The same with ufw (default incoming policy `deny`; rules
match in order):

```bash
sudo ufw allow in on enp1s0f0np0 from 192.0.2.11 to any port 25000:25112 proto tcp
sudo ufw allow in on enp1s0f0np0 from 192.0.2.11 to any port 32768:60999 proto tcp
sudo ufw deny proto tcp to any port 25000:25112
sudo ufw deny proto tcp to any port 32768:60999
```

Replies to connections the machine makes itself are not new connections, so
these rules do not break outgoing traffic. RDMA traffic bypasses the kernel's
firewall; keep the link itself private.

To see what a running group actually opened, on each machine:

```bash
sudo ss -ltnp
```
