# Status: Independent SGLang CUDA graph phases — 2026-11-09 (branch `integration/0.2.0-on-0.1.11`)

Issue [#236](https://github.com/edurdias/capyctl/issues/236), ADR 0014 §2 and
SPEC §15.3: `engine_config.cuda_graphs` accepts a SGLang-only object with
required boolean `prefill` and `decode` keys. Scalar booleans retain their
existing serialized form and behavior: `false` disables both phases; `true`
and omission retain the engine's native policy. Each object phase set to
`false` independently renders its disable switch and disabled backend; `true`
retains that phase's native policy. Other engines refuse the object with a
configuration error. The extra-argument ownership protections remain unchanged.

The mapping follows SGLang 0.5.22's
[ExecGraph CLI definitions](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/arg_groups/fields/exec_.py):
`--disable-prefill-cuda-graph` and `--disable-decode-cuda-graph` are independent
conveniences for their respective `--cuda-graph-backend-*=disabled` settings.
Snapshot reconstruction, command identity and frozen launch descriptors retain
the object without reducing it to a scalar. Resolved-argument drift checks
continue to require every explicitly disabled phase to stay disabled.
The object form requires the enrolled host's `cuda_graph_phases` capability,
so a supported older host is refused before a command is sent; scalar
booleans keep their existing wire contract. Independent phases in a pre-E1
SGLang revision migrate to the equivalent object without approximation.

T03/T09/T14/T22 regression tests were authored before production changes for
all four phase combinations, scalar compatibility, closed object validation,
non-SGLang refusal, snapshot round-trips, command identity, frozen descriptors
and independent resolved-phase drift. No red or green run was performed by
the worker: the integration owner owns verification after the parallel merge.
Existing legacy-boolean snapshot tests remain applicable. CPU and Fake-engine
tests are not qualification; a managed SGLang 0.5.22 GPU launch with prefill
disabled and decode retained still needs a live check. The direct comparison
reported in #236 is not qualification of this managed launch path.

# Release note: Fixes

- **Independent SGLang CUDA graph phases.** Set
  `engine_config.cuda_graphs: {prefill: false, decode: true}` to disable prefill
  graphs while retaining the engine's native decode graph policy. Both keys are
  required booleans and all four combinations are supported. Existing boolean
  settings are unchanged; other engines reject the object form.
