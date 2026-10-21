# Status: ADR 0032 proposed: settling a cancelled request on evidence about that request — 2026-10-21 (branch `docs/adr-0032-settlement`)

[ADR 0032](../design/adr/0032-cancelled-request-settlement.md) (proposed, awaiting the
owner) looks at why a cancelled request stays counted until its engine reads idle
(SPEC §10, amended 2026-10-01), which under steady traffic can fill the 32 requests a
deployment may hold. It weighs the evidence that could settle one request sooner without
ever settling one the engine may still run, read from the public source of SGLang 0.5.22,
vLLM 0.31.0 and 0.32.0 and llama.cpp v0.7.0 (b11669): per-request evidence under an ID
CapyCTL assigns, engine endpoints listing running requests, abort acknowledgements,
counting arguments and connection-level evidence. Only SGLang gives proof about one
request: its own terminal output after an abort by `rid`. The ADR proposes that for
SGLang and keeps today's rule elsewhere. It also records that today's rule can settle a
request cancelled before it reached the engine's scheduler, and asks the owner seven
questions. Nothing is implemented; no code or tests change. CPU and Fake-engine tests are
not qualification, and the live rows listed in the ADR are needed once it is
implemented.

# Release note: none
