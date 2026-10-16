# Status: `capyctl version` reports the build commit; standalone records engine latency — 2026-10-16 (branch `fix/version-commit-and-standalone-latency`)

Two small 0.1.4 items.

**Build commit.** `capyctl --version` printed only `capyctl 0.1.3`, while the
release tarball's BUILDINFO records the commit (`commit: 345806a5…`), so a
binary could not say which commit it came from. `packaging/release.sh` now
exports `CAPYCTL_BUILD_COMMIT` for the compile (the BUILDINFO commit, `-dirty`
appended for a tree built with `CAPYCTL_RELEASE_ALLOW_DIRTY=1`), and a new
`capyctl version` command prints `capyctl <version>` and then `commit: <sha>`
(`--json`: `{"name", "version", "commit"}`, `commit` null for a build that
recorded none, such as a plain `cargo build`). `capyctl --version` is
unchanged on purpose: `packaging/install.sh` compares its whole output with
`capyctl <version>`, as do `scripts/verify-packaging.sh` and
`scripts/test-install.sh`, and an installer from an earlier release would
refuse a newer binary whose `--version` grew a second line. Keeping that line
and adding a subcommand changes no consumer. The commit is fixed for a given
commit, so rebuilding it stays byte-identical, and the shipped-binary check's
rebuild sees the same exported value. `scripts/verify-packaging.sh` now also
checks that `capyctl version` reports BUILDINFO's commit. The install guide,
SUPPORT.md, the bug report template and the release build section of
`docs/operations/install.md` say where the commit is.

**Standalone engine latency.** Found live 2026-10-10: standalone's latency
read (`GET /management/v1/metrics/latency`, and `latency` in `capyctl status
deployment --json`) carried router series only. `EmbeddedLoad::tick` accepted
each report's load sample into its load table but dropped the engine latency
histograms the same reports carry, so `source: engine` series were absent on
standalone for every engine. A server records a host's reports into its
latency table before the load table (`agent_sessions`); `EmbeddedLoad` now
does the same into its own `LatencyTable` (SPEC §17, M80), and standalone
reads that table in its latency view and for the timing header's engine
family, as a server reads its hosts'. There is still no ingress tier on
standalone: it has no host ingress.

Tests: `standalone_reports_its_engine_latency` (T40, `roles_f1`) boots
standalone with an SGLang-shaped Fake engine at the launch's endpoint whose
`/metrics` carries a time-to-first-token histogram; the latency read shows
the instance as `sglang` with an `engine_time_to_first_token` series, `tier:
engine`, `source: engine`, count 2. It failed before the change (no instance
in the read at all). `version_keeps_the_installer_line_and_adds_the_build_commit`
(`terminal_output`) checks that `--version` is exactly `capyctl <version>`,
that `capyctl version` starts with that line and that its JSON carries the
recorded commit; it passes with and without `CAPYCTL_BUILD_COMMIT` set for the
build. CPU and Fake-engine tests are not qualification; the live check still
needed is a standalone run on a native vLLM or SGLang engine reading engine
series from the latency view.

# Release note: Fixes

- **`capyctl version` shows the commit.** `capyctl version` prints the version
  and the commit the release was built from (`--json` for a JSON object);
  include it in bug reports. `capyctl --version` still prints only `capyctl
  <version>`.
- **Engine latency in standalone.** `GET /management/v1/metrics/latency` and
  `capyctl status deployment --json` now show the engine's own latency
  distributions (`tier: engine`, `source: engine`) for standalone instances
  of vLLM, SGLang and TensorFold (llama.cpp exports none), as they do on a
  server with hosts. Before, standalone showed CapyCTL's router series only.
