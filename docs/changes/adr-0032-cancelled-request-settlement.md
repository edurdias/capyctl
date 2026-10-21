# Status: ADR 0032 accepted: settling a cancelled request on evidence about that request — 2026-10-21 (branch `docs/adr-0032-settlement`)

[ADR 0032](../design/adr/0032-cancelled-request-settlement.md) records the owner's
decisions of 2026-10-21 on cancelled requests, which today stay counted until their
engine reads idle (SPEC §10, amended 2026-10-01) and under steady traffic can fill the 32
requests a deployment may hold. From the public source of SGLang 0.5.22, vLLM 0.31.0 and
0.32.0 and llama.cpp v0.7.0 (b11669), only SGLang gives proof about one request: its own
final output after an abort by `rid`.

The decisions: on SGLang, CapyCTL sends its own request ID (the `X-CapyCTL-Request-Id`
value, never the caller's) as `rid`, also through a host's ingress (amending the note of
2026-10-19). On a client hang-up, a deadline cut or a stream idle cut, it aborts that
`rid` on loopback (on a host through a new host-agent operation), keeps reading, and
settles on that request's final output within the stream idle bound, falling back to
today's rule. The answer's `id` stays CapyCTL's request ID. Other engines keep today's
rule, as amended separately on 2026-10-21: an idle reading settles only a request the
engine produced output for. When requests held until the next stop alone fill a
deployment's limit, CapyCTL restarts the instance, journals the restart, and settles them
on its verified exit, never on a timer.

The ADR gives the plan for 0.1.7 and four live rows on host A with SGLang 0.5.22.
Nothing is implemented; no code or tests change. CPU and Fake-engine tests are not
qualification, and those live rows are still needed.

# Release note: none
