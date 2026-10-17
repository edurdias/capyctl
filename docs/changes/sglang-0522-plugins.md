# Status: SGLang 0.5.22 launches: its own plugins closed by selection — 2026-10-17 (branch `fix/sglang-0522-plugins`)

Found live on host A on 2026-10-17 with the approved SGLang 0.5.22
environment: every launch failed `sglang_startup_failed:
plugin_closure_failed`. SGLang 0.5.22 requires `smg-grpc-servicer`
(`Requires-Dist: smg-grpc-servicer>=0.9.0` in its `METADATA`); the installed
0.14.0 registers two `sglang.srt.plugins` entry points, `smg-servicer ->
smg_grpc_servicer.sglang.plugin:register` and `smg-zmq ->
smg_grpc_servicer.sglang.zmq_plugin:register`. The runtime guard
(`runtime/sglang_startup_guards.py::enforce_closed_plugins`) refused any
installed plugin, so it raised `external_plugins_present`. SGLang 0.5.21's
`smg-grpc-servicer` 0.13.0 registers no entry point. Recorded as an ADR 0008
amendment (2026-10-17); the plugin closure's intent (owner decision
2026-09-23, SPEC §13.3, T21) is unchanged.

SGLang's loader, read on host A
(`sglang/srt/plugins/__init__.py::load_plugins_by_group`; the 0.5.20 and
0.5.21 environments have the same whitelist lines):

```python
    allowed_set: set[str] | None = None
    allowed_str = envs.SGLANG_PLUGINS.get()
    if allowed_str:
        allowed_set = {x.strip() for x in allowed_str.split(",") if x.strip()}
    ...
    for ep in discovered:
        if allowed_set is not None and ep.name not in allowed_set:
            logger.info("Skipping plugin %s (not in SGLANG_PLUGINS)", ep.name)
            continue
        ...
        try:
            func = ep.load()
```

`load_plugins()` (scheduler, detokenizer, data-parallel controller, engine)
goes through it; the diffusion loader reads the same variable. The platform
path loads directly only when `SGLANG_PLATFORM` is set.

- **Selection.** `SglangLaunch::render_for_launcher` renders
  `SGLANG_PLUGINS=capyctl-loads-no-plugins` (`capyctl_config::engine_env::
  SGLANG_NO_PLUGINS`) into every SGLang command, single launches and group
  ranks, the adapter's and the controller's native handoff alike; SGLang's
  spawned processes inherit it. The agent's SGLang capability probe gets it
  too, since its guard runs the same closure.
- **Reserved names.** `SGLANG_PLUGINS`, `SGLANG_PLATFORM` and the `SMG_`
  prefix are owned names (`engine_env_reserved:<name>`), so neither a profile
  nor a deployment sets them; an approval glob `SMG_*` is refused.
- **Guard.** Installed general plugins are accepted only when
  `SGLANG_PLUGINS` is exactly that selection and each comes from a
  distribution SGLang requires (`smg-grpc-servicer`, name normalized) and
  none is named as the selection. Still refused: any other `SGLANG_PLUGINS`,
  any `SGLANG_PLATFORM`, any `SMG_*` variable (smg's zmq plugin activates on
  `SMG_SGLANG_ZMQ_HANDSHAKE`), any `sglang.srt.platforms` entry point, a
  plugin from another distribution, `native_already_imported`, an unreadable
  inventory. The module docstring records the 0.5.22 contract.

Tests: `runtime/tests/test_sglang_startup_guards.py` (smg-grpc-servicer's two
entry points pass under the selection and are refused without it or with an
empty one; an unknown distribution's plugin, alone or beside smg's, a plugin
named as the selection and a platform entry point are refused under it;
other selections, `SGLANG_PLATFORM` and `SMG_*` variables are refused; a real
spawn's `__mp_main__` guard passes with the inherited selection and refuses
with the handshake variable; no plugin target is imported); four of them fail
on the previous guard. `runtime/tests/test_sglang_entry.py` (the entry's
`main` launches with smg's plugins under the selection and renders
`plugin_closure_failed` without it or beside another distribution's plugin).
`capyctl-adapters` `sglang_args`, `sglang_group_args`, `sglang_initialize`
(the rendered and spawned environments carry the selection);
`capyctl-config` `engine_env` (the names are reserved; the Rust selection
matches the guard's); `capyctl-agent` installation (only the SGLang probe
carries it). CPU and Fake-engine tests only; not qualification.

Live on host A, 2026-10-17, this branch built there, an isolated standalone: Clef-Flash
(`Cloudflare/clef-flash@8b2e5fd1`, BF16) on SGLang 0.5.22 as an `api: decisions` deployment
was ready in 165 s; its engine log shows `Skipping plugin smg-servicer (not in SGLANG_PLUGINS)`
and `Skipping plugin smg-zmq (not in SGLANG_PLUGINS)`; a decision through the router answered in
0.11 s, and capyctl-bench's decisions run completed (AG News 89.4%, BoolQ 89.6% on its pinned 500
rows each). Qwen3-4B on SGLang 0.5.21 as a chat deployment was ready in 108 s and answered,
unchanged.

# Release note: Engines

- **SGLang 0.5.22 launches.** SGLang 0.5.22 installs plugins of its own
  (from `smg-grpc-servicer`, which it requires), and every launch failed
  `plugin_closure_failed`. CapyCTL now starts SGLang with
  `SGLANG_PLUGINS=capyctl-loads-no-plugins`, a selection that names no plugin,
  so SGLang loads none, and accepts those installed plugins under it. A
  plugin from any other package is still refused. `SGLANG_PLUGINS`,
  `SGLANG_PLATFORM` and `SMG_*` are now names CapyCTL owns
  (`engine_env_reserved:<name>`).
