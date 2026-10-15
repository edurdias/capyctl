# Status: SGLang saver observation under a long state directory — 2026-10-14 (branch `fix/observation-socket-path`)

Found live 2026-10-14 on host A (GB10, standalone, SGLang 0.5.21, `deep`
deployments, build d4b86843): every `capyctl park deployment` of two SGLang
deployments was refused at once with `park refused before any effect (the
engine is not quiescent)`, and the role's `<state>/observation/` directory
stayed empty while the engines served. The same model and recipe parked on
2026-10-05 (ADR 0014 A17 evidence).

Root cause, confirmed in code and by the tests below. The state directory was
`/home/edurdias/capyctl-0.1.3-live-runs/2026-10-14-1854-b18/state`, so the
launch's socket `<state>/observation/<binding>.sock` was 108 bytes. The
protected entry's `entry_environment` (`runtime/sglang_observation_enrollment.py`)
refused any such path over 107 bytes (the `sun_path` limit) by returning
`False` without a word, so the launch ran without an enrolled saver
observation (SPEC §9.2). With no record in the observation directory, every
embedded park failed SPEC §10 step 4 and was refused as not quiescent. Host
roles share the layout, so any role whose state directory is longer than
63 bytes was affected the same way.

Fix:

- The listener binds, and the host connects, through a descriptor of the
  socket's directory, `/proc/self/fd/<fd>/<binding>.sock`, so the address fits
  `sun_path` whatever the directory's length
  (`sglang_observation_server._Custody.address`,
  `capyctl_launchers::native_observation::connect`). The listener now binds in
  exactly the directory its custody verified. The socket is still a 0600 file
  in the 0700 owner-only directory, with the same peer-credential and key
  checks; no abstract address is used. Paths are bounded at 4095 bytes.
- A scope the entry cannot publish now names why on the engine's log, with a
  closed code and no path: `capyctl_saver_enrollment_refused`, stage `scope`
  (`weight_restore`, `directory_path`, `group`, `directory_mode`, or the error
  class).
- An embedded park refused because the launch's observation never enrolled
  (a readable private directory without the binding's record) now says `the
  engine's saver observation is not enrolled` instead of `the engine is not
  quiescent` (`Quiescence::unproven`, `Unproven::ObservationNotEnrolled`). A
  host's `sglang_park_not_quiescent` log event gains `unenrolled`.

Tests (T22, T20):

- `runtime/tests/test_sglang_observation_enrollment.py`
  `test_a_long_observation_directory_still_enrolls` (a socket path over 108
  bytes enrolls, writes its record and answers a keyed observation) and
  `test_an_unpublishable_scope_names_its_reason`: both failed before (the
  scope was refused, and nothing was logged) and pass after.
- `capyctl-launchers` `native_observation::tests::a_socket_under_a_long_directory_is_observed`
  (the Rust client against the real Python listener under a 108+-byte path):
  failed before, passes after.
- `capyctl-agent` `saver_source::tests::an_embedded_sglang_without_evidence_is_not_quiescent`
  (extended) and `only_a_readable_directory_without_a_record_is_unenrolled`;
  `capyctl-controller` `an_unobserved_engine_park_refusal_says_it_is_not_enrolled`.

CPU tests only; they are not qualification. Live check still needed: a `deep`
SGLang park and wake on a role whose state directory makes the socket path
longer than 107 bytes.

## Engine log redaction keeps model paths

Found in the same live run: SGLang's `server_args` line in the engine log
read `'model_path': '<redacted>.8-27B-NVFP4@482ca0f3...'` and
`'/home/edurdias/.cache/huggingface/hub/models--RadixArk--Qwen3.<redacted>'`.
The suspected URL-userinfo rule was not the cause: `redact_urls` only touches
text after a `scheme://`, and the probe showed it left both lines unchanged.
The long-run rule was the cause, in both the Python scrubber inside the engine
(`runtime/sglang_startup_guards.scrub`) and the log writer
(`capyctl_adapters::vllm::args::redact_text`). A run starting with `/` was
kept as a path, but a `.` in the path (`.cache`, `capyctl-0.1.3`, `Qwen3.6`)
ended the run. The rest of the path then began without `/`, and when it was
48 characters or more it was blanked as a credential. The Python scrubber also
blanked any whole path of 48 characters or more. Now a run that starts with
`/` makes the rest of its word a path, past any `.`, `@` or `:`, until
whitespace, a quote or bracketing punctuation. A path is kept segment by
segment, so a credential-length segment inside one is still blanked. Launch
keys (by value), URL userinfo, query strings and bearer values are redacted
as before.

Tests (T37), failing before and passing after:
`engine_log::tests::model_paths_in_the_argument_echo_are_kept` and
`runtime/tests/test_sglang_startup_guards.py`
`test_scrub_keeps_model_paths_and_still_redacts_keys`. Both use the two live
paths and also check that the keys on the line, a 64-character segment inside
a path, URL credentials and bearer values are still redacted. CPU tests only.
Live check: the `server_args` line of the next SGLang launch shows its model
path.

# Release note: Fixes

- **Deep parking of SGLang failed on hosts whose state directory path is
  long.** When the state directory's path was longer than about 63 bytes,
  the engine's memory-saver observation could not start, and every park of
  an SGLang deployment was refused as "not quiescent". The observation now
  works with a state directory of any length. If it still cannot start, the
  engine log says why, and a refused park says the engine's saver
  observation is not enrolled.
- **Engine logs no longer hide model paths.** A model path containing a
  `.` (for example under `~/.cache/huggingface`) showed up in SGLang's
  argument line with part of it replaced by `<redacted>`. Paths are now kept
  whole. A credential-length segment inside a path, keys, URL credentials and
  bearer values are still redacted.
