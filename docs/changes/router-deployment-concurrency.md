# Status: Router follows deployment concurrency — 2026-11-10 (branch `integration/0.2.0-on-0.1.11`)

Issue #240, SPEC §10 and ADR 0014 §2: the router's per-deployment in-flight
bound now follows `engine_config.max_concurrent_requests`, below and above
32, with 32 still the default when undeclared. All instances share the bound;
chat (streaming and collected), decisions and embeddings use it for admission
and durable lease grants. The restart for cancelled requests held until the
next verified stop uses the same bound (ADR 0032 §7). The management load read
reports the configured router bound separately from derived or sampled engine
parallelism, which remains only a routing hint.

The server reads the count directly from its durable effective revision; no
protocol field, capability, command digest or durable encoding changes. Under
ADR 0017 §§2–3, a 0.2.0 server can therefore use this pre-existing deployment
setting with a supported 0.1 host. The host ingress's unrelated safeguard of
64 concurrent forwards in total is unchanged.

Tests were authored before the production changes for configured counts of
2 and 48 and the undeclared default: standalone configuration through router
admission and durable leases (T19, T47), real server/host ingress routing of
48 concurrent chat requests and a two-slot embeddings deployment (T19, T47),
the 0.2.0/0.1.11 protocol policy (T34), and held-request restarts (T17, T19).
Additional decision-routing and capacity-report regressions cover T43 and T19.
Red and green runs, builds, formatting and linting were deliberately deferred
to the integration owner; no passing-test claim is made here. CPU and
Fake-engine tests are not qualification. A live run with declared concurrency
48 on an authorized engine, including a rolling server-first upgrade, remains
the native-engine check.

# Release note: Fixes

- **Use every configured request slot.** `engine_config.max_concurrent_requests`
  now bounds requests in flight through CapyCTL per deployment, including chat,
  decisions and embeddings, instead of always capping them at 32. Leaving it
  unset keeps 32. The bound applies across all instances, locally and on remote
  hosts, and is shown in the management load read.
