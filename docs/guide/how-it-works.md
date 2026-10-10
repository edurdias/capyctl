# How it works

CapyCTL sits between your apps and the vLLM, SGLang, TensorFold or llama.cpp engines on
your GPUs. Apps see one OpenAI-compatible endpoint. Behind it, CapyCTL starts engines, decides
which models hold GPU memory, and moves the rest out of the way.

![How requests reach a model through CapyCTL](how-it-works.svg)

## One machine or several

On one machine, `capyctl start standalone` runs everything in one process: the
endpoint, the scheduler and the engines. This is where most people start.

With several machines, one machine runs `capyctl start server` and each GPU
machine runs `capyctl start host`. The server holds the endpoint and decides
where each model runs; the hosts start and stop engines when the server asks.
Hosts and server talk over your private network with mutual TLS. Commands are
the same in both setups; with several machines you run them on the server.

A discrete card and a unified-memory machine use the same steps. CapyCTL reads
the GPU at start. On a discrete card it counts the card's memory and host RAM
separately; on unified memory it counts the one shared pool.

## Why one endpoint

Your apps keep one base URL and one API key, and pick a model by the name in
the request. They do not need to know which machine runs a model, whether it
is awake, or which port its engine uses. The router sends each request to the
engine that serves it, waking the model first if it is parked.

## Ready, parked, stopped

![Where a model's memory goes when it is ready, parked or stopped](model-states.svg)

A **ready** model holds its weights and KV cache on the GPU and answers at
once.

A **parked** model's engine keeps running, but gives back its GPU memory.
There are two ways to park:

- **In host RAM**: the weights are copied to host RAM, and a wake copies them
  back. This takes seconds. It is the default on a discrete card when the copy
  fits in RAM.
- **Deep**: the weights are dropped, and a wake reloads them from disk. This
  is the way to park on unified memory, where a copy would come out of the same
  pool it is meant to free.

A **stopped** model has no engine and holds no memory. A request for it is
refused until you start it again.

## Switching

When a request asks for a parked model and the GPU is full, CapyCTL parks the
idle model that is in the way, wakes the one asked for, and then answers. It
waits for requests in progress to finish before it parks anything. If a
parked copy would not fit in host RAM, CapyCTL stops that model instead and says
so.
[How requests wait and models switch](parking.md#how-requests-wait-and-models-switch)
gives the order of events, the settings and the errors.

## The memory ledger

The memory ledger is CapyCTL's count of how much GPU memory and host RAM each
model holds or has been promised. CapyCTL checks it before every start, wake and
switch, so two models never count on the same memory. When CapyCTL cannot tell
whether memory was released, it keeps counting it as used.

## What CapyCTL never does

- It does not install engines. You register the vLLM, SGLang, TensorFold or llama.cpp you
  already have with `capyctl engine add`.
- It does not touch GPU drivers, CUDA or system packages.
- It does not expose engines. They listen on loopback with a key made for each
  launch; the endpoint is the only way in.
- It does not delete your model files. It downloads weights only when a
  deployment names a Hugging Face or HTTP source.

Next: [Install](install.md).
