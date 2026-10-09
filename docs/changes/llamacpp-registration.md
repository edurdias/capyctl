# Status: llama.cpp registration (plan slice L1) — 2026-10-09 (branch `feat/llamacpp-l1-registration`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §1–§2, plan slice L1
(`docs/plans/2026-10-09-llamacpp-engine.md`). `llamacpp` is the fourth engine kind
in every closed engine set, and a bare `llama-server` binary registers beside the
dist-info installations. SPEC §20 gains T42 (llama.cpp conformance).

- **Kind.** `Engine::Llamacpp` (serde and CLI name `llamacpp`), in `Engine::ALL`;
  the environment profile `local-llamacpp`. `capyctl-config/src/llamacpp.rs` holds
  the executable name, the version-line parser, the `<version>+<commit>`
  fingerprint, the release tag commits (`0.6.0` → `d812350`) and the
  `/etc/llama.cpp/config.ini` check under a replaceable system root.
- **Detection** finds an executable regular `llama-server` in `PATH` entries,
  `~/llama.cpp/build*/bin`, `/opt/*/bin`, `/usr/local/bin` and under `--path`,
  follows no link out of the scanned root, reads the version from a
  `libllama.so.X.Y.Z` name beside it (or in `<prefix>/lib`), else `unknown`, and
  runs nothing.
- **`engine add`** takes the binary or its directory (a link is refused, naming its
  target), runs the bounded version check reading standard error, parses
  `version: <v> (build <n>, commit <h>)` and writes `build_fingerprint: <v>+<h>`
  (the build number is dropped) with `security.deep_park: disabled` and no
  `cuda_home`. A version line on standard output alone does not parse
  (`engine_unsupported`); `--deep-park enabled` is `capability_missing`; a machine
  with `/etc/llama.cpp/config.ini` is `engine_unsupported` naming the file, before
  the binary runs. Nothing is written for any refusal. Nothing is probed: the
  capability report says deep parking is missing.
- **Digest** (ADR 0008): the binary and every `lib*.so*` entry beside it, and in
  `<prefix>/lib` for `<prefix>/bin/llama-server`; a link counts by its target text.
- **Listing.** `custom` follows the verified set read through the release a build
  counts as: `0.6.0-dev` with the tag's commit counts as `0.6.0`. Listings show a
  llama.cpp profile's fingerprint rather than the library version the host
  reports. The verified set has no llama.cpp entry yet (ruling 2: it lands with
  the live rows), so every build lists as `custom`.
- **The role's own engine.** `local_engine.llamacpp`, `--llamacpp-bin` and
  `CAPYCTL_LLAMACPP_BIN`, with the shared precedence; the profile is `local`, or
  `local-llamacpp` beside another engine. Its version is read from standard error,
  it never parks, and it is refused while the system `config.ini` exists.
- **Until slices L2–L5 land**, a deployment on a llama.cpp profile is refused at
  resolution, every llama.cpp option (host-fixed, extra or rendered) is refused so
  the option policy fails closed, a parking residency on a llama.cpp profile is
  `capability_missing`, and the launch, park and group paths refuse the kind.

Tests (T42 with T01, T03, T07, T14, T21, T22, T37): `capyctl-config/tests/llamacpp.rs`
(kind, version line, listing rule under a test verified table, deep park, options
failing closed, the config file, the resolution refusal), `tests/registration.rs`
(a release before ADR 0029 skips a `llamacpp` profile as an unknown kind; this one
loads it), `engine_settings` unit tests (three ways, profile names, the host's own
llama.cpp), `capyctl-agent/tests/engines.rs` (detection runs nothing, links out of
the root, resolution, the version check on standard error),
`installation` unit tests (the digest over the libraries and `<prefix>/lib`),
`capyctl-cli/tests/engine_cli.rs` (`engine add` and its refusals),
`tests/grammar.rs` (`--llamacpp-bin`) and the standalone provider's own llama.cpp.
CPU and Fake-engine tests only; they are not qualification. Only the live rows
LC1–LC6 (slice L6) qualify llama.cpp.

# Release note: none
