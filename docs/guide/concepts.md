# Concepts

## Deployments and routes

A deployment is one model you want mllm to serve: the checkpoint, the engine
installation that runs it, its engine settings, and the routes it answers.
A route is the `model` name clients put in their OpenAI requests. You write a
deployment as a YAML document and hand it to `mllm deploy model`.

## Hosts and the server

A host is a machine with GPUs that runs engines. The server accepts API
requests, decides which host runs each model and forwards requests to it.
Hosts connect to the server over gRPC with mutual TLS. Standalone is a server
and one host in one process.

Engines only ever listen on loopback on their own host. Clients talk to the
server's endpoint, never to an engine.

## Instances

A deployment runs as one or more instances. Each instance is one engine
process on one GPU of one host. Models that need several GPUs are not
supported yet.

## Parking

A parked instance keeps its engine process but gives GPU memory back. How much
it gives back is the deployment's `residency`:

- `deep`: weights and KV cache leave GPU memory. Waking reloads the weights.
  This uses engine controls that mllm keeps on loopback behind a key generated
  for each launch; a host can turn them off, and then `deep` is refused on it.
- `host_backed` (shallow): weights move to host memory and come back on wake.
  Refused on machines where GPU and system memory are one pool, because it
  would free nothing.
- `restart_only`: never parks. The model is stopped and started instead.

A request for a parked model wakes it. The request waits; it is not dropped.

## Switching

When a request needs a model that is not running and the GPU has no room, mllm
parks or stops an idle model to make space, then starts or wakes the one that
was asked for. Models with requests in flight are not chosen.

## The memory ledger

Each host reports its GPUs and the memory it lets mllm use. The server keeps a
ledger of every piece of GPU memory it has handed to a model, and starts a
model only when the ledger says it fits. If mllm cannot tell whether an engine
still holds memory, for example after a host lost its connection, it keeps
that memory booked until it can check, rather than risk starting two models
into the same memory.
