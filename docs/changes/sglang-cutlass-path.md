# Status: SGLang 0.5.22 finds `cutlass` without running its `.pth` hook — 2026-10-19 (branch `fix/sglang-cutlass-path`)

Found live on host A on 2026-10-19: SGLang 0.5.22 under CapyCTL failed to
start Qwen3-4B-Instruct (dense, BF16). Its scheduler raised
`ModuleNotFoundError: No module named 'cutlass'` from
`sglang/srt/layers/dual_gemm.py::_select_mode`, which imports
`sglang.kernels.ops.gemm.cutedsl_dual_gemm` and with it `cutlass`. The same
installation launched directly worked. `cutlass` belongs to
nvidia-cutlass-dsl 4.8.0 (SGLang 0.5.22 requires it), which installs it under
`<site-packages>/nvidia_cutlass_dsl/dsl_packages/` and adds that directory to
the search path only through the code hook `nvidia_cutlass_dsl_packages.pth`.
CapyCTL starts SGLang under `python -IS` and composes the search path itself
without running `.pth` hooks (SPEC §9.2), so the directory was never added.

ADR 0008 amendment 2026-10-19: the composition
(`runtime/sglang_startup_guards.py::trusted_search_path`) appends
`nvidia_cutlass_dsl/dsl_packages` after the launcher-selected site-packages
when both components are real directories inside it (neither a symlink). No
`.pth` file is read or run, nothing is imported to compute it, and the plugin
closure enumerates it like any other entry. The entry
(`runtime/sglang_entry.py::_compose_search_path`, before the plugin closure
and again at the guarded import) and the SGLang capability probe
(`runtime/engine_capabilities.py`) use it. SGLang's scheduler and detokenizer
are spawned; CPython spawn hands each child the parent's `sys.path` before it
runs the protected main script, so they import from the same composition.
No other `.pth` path is honoured.

Tests (`runtime/tests/test_sglang_startup_guards.py`,
`CutlassDslSearchPathTests`, T21): a fresh venv laid out as
nvidia-cutlass-dsl installs, with the vendor hook plus a marker write. Under
`-I` alone the hook runs (positive control); under `-IS` the entry's
composition appends exactly site-packages and `dsl_packages`, `import cutlass`
loads the package from `dsl_packages` in the entry's process and in a real
spawn child with the entry as main (both under `-IS`), a stdlib module of the
same name in `dsl_packages` does not shadow the stdlib, and the marker is never
written. A `dsl_packages` symlinked outside the tree is not added and
`import cutlass` fails, as it did for every installation before this change;
a symlinked `nvidia_cutlass_dsl` or a `dsl_packages` that is a file is not
added either. CPU and Fake-engine tests are not qualification.

Found and checked live on host A, 2026-10-19: before this change, Qwen3-4B-Instruct (BF16)
on SGLang 0.5.22 through CapyCTL `main` (`07ee2ea6`) failed to start (scheduler
`ModuleNotFoundError: No module named 'cutlass'` from `sglang/srt/layers/dual_gemm.py`),
while the same venv launched directly served it. With this branch (`e63dd670`) the
deployment initialized and served 4 concurrent streams; when their clients hung up, SGLang's
`num_running_reqs` went from 4 to 0 within 0.15 s (0.13 to 0.14 s for SGLang launched
directly), and CapyCTL recorded the cancellation as acknowledged once the engine was idle.

# Release note: Fixes

- **SGLang 0.5.22 finds its `cutlass` package.** SGLang 0.5.22 imports
  `cutlass` from nvidia-cutlass-dsl, which only a `.pth` hook puts on Python's
  path, and CapyCTL never runs those hooks; serving a dense BF16 model failed
  with `No module named 'cutlass'`. CapyCTL now adds that one directory inside
  the installation to the engine's path itself, still without running the hook.
