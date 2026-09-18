# Live evidence: F2 on host-a

Purpose: this is the record of what was actually observed when mllm drove a real
inference engine on real hardware. Nothing else in the repository can stand in for
it — the CPU and Fake-engine suites prove that the controller's machinery holds
together, and passing them establishes nothing about a native recipe (SPEC §18).

Every run of `scripts/live/run-on-spark.sh` appends one entry below, newest last.
An entry names the commit it ran at, so the code that produced a result can be
recovered; it never carries the output itself, which lives in
`target/live/<stamp>/` on the machine that ran the script.

Entry template:

```markdown
## 2026-MM-DD — S1 run N — <commit>
vLLM 0.29.0, qwen3-4b-instruct, host-a (GB10, 121 GiB unified). Command: scripts/live/run-on-spark.sh

| Scenario | Result | Timing / sample |
| --- | --- | --- |
| L1 launch | pass | cold start to Ready: NN.N s |
| L2 serve | pass | plain NN.N s, streaming NN.N s |
| L3 access control | pass | engine on 127.0.0.1:NNNN only |
| L4 stop | pass | NN.N s, group empty |
| L5 restart | pass | NN.N s, new incarnation |
| L6 bad source | pass | closed in NN.N s |
| L7 recovery | pass | Ready in NN.N s |
| L8 engine exits at once | pass | closed in NN.N s, no leftovers |
| L9 deadline bound | pass | start refused, nothing launched |
| L10 no engine | pass | NoEngineInstallation; release binary clean |
| L11 memory returns | pass | before NN.NN GiB, after NN.NN GiB |

Failures and what changed: …
vLLM control routes keyed by API key: yes/no (see the auth-scope finding below).
```

## 2026-09-18 — S1 run 1 — pending

Not yet run. The suite, the runner and this runbook were committed first so that
the run has something to report against; the run itself is the next step and will
replace this heading with the template above.

### vLLM auth scope, verified on host-a on 2026-09-17

vLLM 0.29's own authentication middleware guards a fixed list of path prefixes.
From `vllm/entrypoints/serve/middleware/authenticate.py`:

    GUARDED_PREFIX = ("/v1", "/v2", "/inference", "/cohere")

Everything outside that list is unauthenticated, which includes exactly the routes
mllm depends on for deep park: `/sleep`, `/wake_up`, `/is_sleeping` and
`/collective_rpc`. On a development-mode server those are open to any local caller.
SPEC §9.1 and T21 require that surface to be denied by default with explicit opt-in
only, so shipping on vLLM's own middleware would not have met the requirement.

mllm closes the gap with its own ASGI middleware, `runtime/mllm_vllm_guard.py`,
loaded into the engine process through vLLM's `--middleware` flag. It requires a
valid `Authorization: Bearer <VLLM_API_KEY>` on every HTTP and WebSocket path except
`/health`, with `OPTIONS` passed through unchecked because a preflight is not a
control action. The presented token is compared by SHA-256 digest with
`secrets.compare_digest`, mirroring vLLM's own check, so neither length nor timing
leaks anything about the configured key.

Scenario L3 is what holds this true over time: it asserts that an unkeyed `POST` to
`/sleep` and to `/collective_rpc` is refused with 401, and that a `GET /is_sleeping`
carrying the key the launch minted is answered. A regression that dropped the
middleware would leave those routes open and L3 would fail.

### What run 1 must confirm about L9

L9 was specified as a readiness deadline: a healthy engine still loading when its
deadline arrives is terminated, proven gone, and the deployment closed. Reading the
path shows that shape is not reachable from a deployment document. An administrative
start is scheduled at a fixed ten-minute activation window, and `accept_start`
refuses any operation whose deadline is further out than the deployment's own
`request_deadline`, so a twenty-second deadline is refused at admission rather than
expiring mid-load. The readiness bound itself is `min(step deadline,
initialize_timeout)`, and `initialize_timeout` has a thirty-second floor and a
nine-hundred-second standalone setting that no document can lower.

The scenario as committed asserts the bound that does exist — the refusal is
definite, nothing was launched, and nothing is owed. Exercising the readiness
timeout against a live engine needs a lever that does not exist yet, and deciding
whether to add one is work for after run 1.
