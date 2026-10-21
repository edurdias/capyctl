# Status: Discrete GPUs: the startup peak is measured per memory domain — 2026-10-21 (branch `fix/discrete-startup-peak`)

Owner decision 2026-10-21; ADR 0019 amendment A2, with ADR 0014 amendments A2
(measured startup), A12 (kernel builds) and A13 (process identities). Found
during the discrete container qualification on the 16 GB laptop card: on a
discrete GPU CapyCTL never measured a deployment's startup peak. The cold phase
charges two domains, the card and host RAM (ADR 0019 §3), and the measurement
ran only for a cold phase of one allocation (`StartupPeak::new` in the
coordinator, `measurable` in `capyctl-config`), so every start kept the
estimate (`startup.provenance` `default`).

What changed:

- `capyctl_config::effective::startup_measure` replaces `measurable`: one pool
  is measured as before; a discrete cold phase of one card and one host RAM
  allocation is measured per domain.
- The coordinator samples the host's processes with its availability while an
  Initialize runs on a discrete GPU and, once the engine is ready, matches them
  to the process identities it reported: the card's peak is the most GPU memory
  the driver attributed to them in one sample, host RAM's the most resident
  pages (`RssAnon` + `RssShmem`). Another program's memory is not counted.
  Samples inside a kernel build and a start beside another activation on its
  host record nothing, as on unified memory.
- `startup_measurements` is keyed by memory domain (store schema v52; each
  earlier peak keeps the domain of its revision's one cold allocation, read
  from the frozen revision). A start freezes each measured domain's peak into
  its plan (`startup_domains`), never below that domain's steady charge; a
  domain not yet measured keeps its estimate. A declared `memory.startup` or
  `resources:` block is never replaced.
- A remote host is sent the card's figure as `SingleLaunchPlan.startup_bytes`
  and host RAM's as the new `startup_host_bytes` (field 18, capability
  `startup_host_bytes`, ADR 0017), and charges each on its own domain. A host
  without the capability is refused `host_capability_missing:startup_host_bytes`
  for such a launch before anything is sent.
- Status: `startup.device_provenance` and `startup.host_provenance` beside
  `startup.device_bytes` and `startup.host_bytes` (deployment and starting
  instance), `startup.provenance` the card's, and each `startup.measured`
  record names its `domain`.

Tests (T14 T26): `a_discrete_startup_peak_is_measured_per_memory_domain` and
`a_discrete_domain_keeps_its_estimate_until_it_is_measured` (coordinator, both
failing before: nothing was recorded on a discrete host),
`v52_keys_startup_peaks_by_domain_and_carries_rows_across` (store migration),
`a_launch_carries_the_host_ram_startup_charge_only_beside_the_cards` and the
capability cases in `every_post_baseline_field_names_its_capability`
(protocol). The unified startup tests pass unchanged. CPU and Fake-engine tests
are not qualification.

Live check still needed (discrete-GPU laptop, vLLM): deploy a model stating
only `name`, `engine` and `model` (for example Qwen3-4B on vLLM 0.31.0), start
it once and check `capyctl status deployment <name> --json`: `startup.measured`
has one record on `gpu0` and one on `system`, the card's close to what
`nvidia-smi` showed for the engine's processes during the load and the host
RAM's close to their peak `RssAnon` + `RssShmem`, and `startup.device_provenance`
and `startup.host_provenance` are `measured`. Stop and start it again: the
starting instance's `startup` reserves those figures (each at least the steady
charge), it reaches Ready, and the STARTUP column shows the measured card
figure with the measured host RAM beside it. Repeat once in the container
setup of the discrete container qualification.

# Release note: Memory

- **On a discrete GPU the startup peak is measured.** A first start now
  measures the card and host RAM apart, from the engine's own processes, and
  later starts set aside what was measured instead of the estimate, on each
  domain once it is measured. `capyctl status deployment <name> --json` says
  where each figure came from (`startup.device_provenance`,
  `startup.host_provenance`). An enrolled host with a discrete GPU needs this
  release to start a deployment whose peak was measured that way
  (`host_capability_missing:startup_host_bytes` otherwise).
