# Status: Standalone reports its own engine load — 2026-10-08 (branch `fix/standalone-load-reports`)

Owner decision 2026-10-08 (SPEC §§10, 17, ADR 0013 §10, D9): an external
router needs the engine's running and waiting counts and its real running
limit on a single machine too. Until now the standalone role sampled nothing:
it composed `GET /management/v1/metrics/load` with no load table
(`remote_roles::load_view(.., None)` in `roles.rs`), and its
`CoordinatorLifecycle` had no load to read, so every embedded instance read
`sample: null`, `max_running` stayed derived (`declared`, `default`, ...), and
the router scored embedded instances on its own in-flight count alone
(`load_source: router_in_flight`). The host agent's `LoadReporter` was the only
sampler, and it read its targets only from a host ingress, which standalone
does not have. This supersedes the "Standalone has no host load report" line of
the load-read entry below.

Standalone now samples its engines in process with the host agent's reporter.
`capyctl_agent::load::LoadSource` names what a reporter samples; the host
ingress implements it as before, and `capyctl_controller::embedded_load`
implements it from the store: every Ready embedded launch of the current
session with open dispatch (`local_ready_launches`, which now carries the
instance index), on its recorded loopback endpoint with its sealed inference
key, under the embedded host's published name. Each second it scrapes
`/metrics`, reads SGLang's `/v1/loads?include=core` once per launch, validates
each report (`LoadReport::try_from`) and records it in its own `LoadTable` with
the same fences a host report meets. The management load read and the router
read that table: `CoordinatorLifecycle::with_embedded_load` gives an embedded
instance its fresh sample by its placed host, and `balance::usable_load` now
matches a sample to the remote host, else the placed host. A sample's
`ingress_in_flight` is the router's count for the instance, since standalone
forwards to its engine directly. Engine latency histograms carried on the
samples are not read in standalone; its latency view is unchanged. The guide's
[reading load](../guide/parking.md#reading-load) section no longer says
standalone has no sample.

Tests: `standalone_reports_its_engine_load` (`capyctl-cli`, `roles_f1`) boots
standalone on the Fake installation, starts a deployment and stands an
SGLang-shaped engine at the recorded endpoint that answers keyed `/metrics` and
`/v1/loads`; the load read shows a fresh sample with the engine's gauges and
`max_running` `{"count": 24, "source": "engine"}`, and the router's candidate
uses that engine load. It failed before (`sample: null`, source `default`) and
passes after. `unusable_samples_fall_back_to_router_in_flight` now expects an
embedded instance's sample to be used when it names the placed host, and not
otherwise. CPU and Fake-engine tests are not qualification.

Live check still needed: rerun live check #2 on host A (standalone, SGLang
0.5.21, `max_concurrent_requests: 2`): during an in-flight request the load
read shows a fresh `sample` with `engine.running` and `max_running.source:
engine`, and the router's candidate logs `load_source: engine`.

# Release note: Standalone

- **Live engine load in standalone.** `GET /management/v1/metrics/load` now
  shows each standalone instance's engine load, about once a second, as it
  does on a server with hosts: the engine's running and waiting requests, its
  KV-cache use and, for SGLang, how many requests it runs at once (`source:
  engine`). CapyCTL also uses that load to choose between instances. See
  [reading load](../guide/parking.md#reading-load).
