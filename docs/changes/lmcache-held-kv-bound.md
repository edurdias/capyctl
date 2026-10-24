# Status: LMCache: a model parked and woken repeatedly no longer runs out of memory — 2026-10-23 (branch `fix/lmcache-held-kv-bound`)

Found live on host A (GB10, SGLang 0.5.22 with LMCache 0.5.6, Qwen3-4B, `restart_only`, a
12 GiB pool and a 5 GiB KV cache): every restart-park added the stopped engine's KV cache to
the LMCache server's GPU memory (5,328 MiB per park), and nothing released it, neither the
wake nor its requests, whether they hit the cache or stored new KV. With the same prompt
every cycle the server held 53,624 MiB at the tenth wake while the launch was charged
20,132,659,200 B; the wake was refused (`residency_blocked insufficient_memory`) and the
router answered 503 after 600 s. With a new prompt every cycle the fifteenth wake failed the
same way at 80,324 MiB. ADR 0033 A13's release (a reading below the KV after the new engine
first stores KV) held on vLLM and never came on SGLang.

Owner decision of 2026-10-23 ("bound it and charge the evidence"), ADR 0033 amendment A15:

- **The charge is never below what the server is seen to hold.** A GPU reading of the server
  above the GPU memory its launch's charge holds in its phase (its device figure, the
  stopped engine's KV while held, or an earlier larger reading) raises every phase's charge
  to it, journaled `lmcache_charge_raised`, while the instance is Ready or parked with no
  lifecycle run open; from the embedded watch and from a host's load reports alike. Only
  A13's release reading lowers it (with the held KV), or the stop. Kept per launch in
  `launch_lmcache.observed_device_bytes` (store schema v57). Status's `charge_bytes`
  includes it. The revision's measurement rules are unchanged.
- **No restart-park while an earlier engine's KV is held.** `park deployment` on a Ready
  instance whose server still holds it (no release reading since the last park) stops the
  instance and its server instead, answered under the park command's receipt (as the parked
  growth stop is); the idle policy stops it (`ready_idle_held_kv_unreleased`); both journal
  `lmcache_server_stopped` with `held_kv_unreleased: …`. The stop releases nothing before
  its cleanup evidence names the engine's and the server's exits; the next activation is a
  new start with a new server. A server so holds at most one stopped engine's KV, which its
  charge counts. Any other park of such an instance is refused.

Guides: parking ("Parking with LMCache") and engines (LMCache memory). A draft upstream
LMCache issue is kept for the owner, not filed.

Tests (T45; failing before, passing after): store — readings above the charge raise it
when Ready, parked with the KV and after the wake, never during a run, lowered only by the
release reading (vLLM and SGLang); a second park while the KV is held is a stop that needs
both the engine's and the server's exits, replayed by the park command, and the next start
restart-parks again (vLLM and SGLang); the idle policy's stop with its journal (vLLM and
SGLang); the measurement callers send readings above the charge; the v57 migration.
Coordinator — the raise from a host's report while parked, the Ready charge after the wake,
and the second park stopping the instance with its server, then a new launch with a new
server, through the embedded adapter's and a remote host's evidence. Existing restart-park
tests now send the release reading before the idle park. CPU and Fake-engine tests only;
they are not qualification.

Live check (host A, GB10, 2026-10-23, this branch's code; evidence kept outside the
repository): 12 park and wake cycles with the same prompt each, SGLang 0.5.22 then vLLM
0.32.0 with LMCache 0.5.6 (Qwen3-4B, 12 GiB pool, 5 GiB KV). All 24 requests answered 200.
SGLang alternated a restart-park (the cache answered 2,048 tokens after the wake) and a stop
of the instance with its server (6 `lmcache_server_stopped`, each server's process gone, a
new server per start); the server never read more than 5,736 MiB (53,624 MiB before the
fix). vLLM held the old KV the same way whenever the request after the wake stored nothing
(5,526 MiB parked, 5,670 MiB after the wake, no release) and stopped at the next park
(4 stops); at the other parks the server held no engine KV and the reading after the wake
released the charged KV (4 `lmcache_kv_released`). In all 114 samples with a server running
the charge covered its GPU reading; it was raised from readings 16 times (for example 5,736
MiB after a SGLang wake against the 5,632 MiB the first launch held). The first SGLang try
failed its first wake at 103 s on the 105 s restart-park wake window
(fixed since in #197); the run used `timeouts.wake: 1200s`.

# Release note: Fixes

- **A model parked and woken repeatedly with LMCache could end up unable to wake.** Each
  time a `restart_only` model with LMCache was parked, its LMCache server kept the stopped
  engine's cache in GPU memory, and on SGLang it never gave it back, so after about ten park
  and wake cycles the machine ran out of memory and the next wake failed. CapyCTL now sets
  aside at least the memory the server is seen to use, and when the server still holds an
  earlier engine's cache, parking stops the model together with its server instead; the
  next request starts it fresh. See [parking](../guide/parking.md#parking-with-lmcache).
