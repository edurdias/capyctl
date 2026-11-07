# Status: A start above a whole managed limit says so and names the setting — 2026-11-06 (branch `fix/241-discrete-system-startup`)

Issue #241. On a discrete GPU (RTX 5090, vLLM 0.32.0, Qwen3.8-27B NVFP4) with a
16 GiB system limit, a first start was charged the 4 GiB engine host overhead
in host RAM, ran, and recorded a 26.3 GiB host-RAM startup peak (`RssAnon` plus
`RssShmem`, memory no page-cache reclaim returns). Every later start reserves a
measured peak (ADR 0015 amendment 2026-09-23, `ordinary_lifecycle/startup.rs`
`frozen_startup`), so the next start needed 26.3 GiB against the whole 16 GiB
limit and was refused `capacity_blocked` with `needs 26.3 GiB of system memory,
16.0 GiB free of its 16.0 GiB limit; wait, stop another deployment, or start
with --evict`. None of that advice can place it, and the setting that can,
`host.resource_policy.memory.system.managed_limit`, was not named. The same
happens on unified memory: a solo first start reserves the whole managed limit,
but its measured drop in availability can exceed it, and the next start is then
refused the same way.

SPEC §14: when a host refuses a start because its need is above a memory
domain's whole managed limit, that domain is the one its reason names, as
`above its whole <limit> limit`, with the setting to raise: standalone's
`host.resource_policy.memory.system.managed_limit`, an enrolled host's
`resource_policy.domains.<domain>.managed_limit`, or, for a standalone card,
that its limit is derived from the card (`capyctl-scheduler`
`placement::shortfall`). On a host offering a device choice, an option that
fits every managed limit leaves the refusal to the limit that did refuse it
(host-KV, parked, count), as no card's limit is to blame. When every refusing
host is above a whole limit (`Unplaceable::beyond_limits`), the store returns
`LifecycleError::CapacityBeyondLimit` instead of `CapacityBlocked`; management
answers it with the same `capacity_blocked` code (503) without the advice to
wait, stop another deployment or `--evict`. Request-driven activation, fault
classification and park deferral treat it exactly as `CapacityBlocked`. A host
that is only full of other work keeps the old reason and advice.

Not changed, for the owner: the first start on a discrete GPU is still charged
only the host overhead in host RAM, so it can run past the system limit
unnoticed before its peak is measured, and a measured peak above the limit
still makes every later start refuse until the limit is raised. Whether such a
start should run alone and reserve the whole limit, or a discrete first start
should estimate its host-RAM peak from the weights, changes the startup budget
rules (ADR 0014, ADR 0015 amendment 2026-09-23). #219 is the card-side mirror.

Tests: `capyctl-controller`
`a_discrete_host_ram_peak_above_the_system_limit_is_refused_naming_the_setting`
(T14 T26) scripts a first start whose processes hold 40 GiB of host RAM against
a 30 GiB system limit, and
`a_unified_peak_above_the_managed_limit_is_refused_naming_the_setting` (T26
T29) a solo first start whose load drops availability by 40 GiB against a 36 GiB
limit; both check the next start is refused `CapacityBeyondLimit` with the
setting named and reserves nothing. On `main` the same scenarios return
`CapacityBlocked` with `needs 40.0 GiB of system memory, 30.0 GiB free of its
30.0 GiB limit` (and of unified memory, 36.0). `capyctl-scheduler`
`a_need_above_the_whole_limit_names_the_setting_and_no_eviction_helps` (T05 T23)
covers standalone, enrolled and card wording and a host that is only full;
`a_device_option_that_fits_the_managed_limits_leaves_the_other_limit_to_explain`
(T23 T27) a device choice where one GPU is above its card's limit and the other
fits, both over a shared host-KV limit: the host-KV limit is named (on `main`
the refusal named the first GPU's card instead);
`a_host_kv_refusal_names_the_limit_and_the_setting_to_raise` now expects the
new wording for its need above the limit. `capyctl-management`
`a_start_above_a_managed_limit_is_not_told_to_wait_or_evict` (T05 T23) checks
the code, status and message. CPU and Fake-engine tests are not qualification;
the live check still needed is the reporter's sequence on a discrete GPU.

# Release note: Fixes

- **A start that needs more memory than a whole limit says which setting to
  raise.** A deployment whose measured startup peak was above a managed limit
  (host RAM on a discrete GPU, where vLLM's loader can hold well beyond the
  weights) could start once and then never again, refused with advice to wait,
  stop another deployment or use `--evict`, none of which helps. The refusal
  now says the need is above the whole limit and names the setting, for
  example `host.resource_policy.memory.system.managed_limit` on standalone
  (issue #241).
