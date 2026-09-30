# ADR 0021 — Terminal output by default, JSON on request

**Status:** Accepted (owner decision, 2026-09-29).
**Supersedes:** the owner decision of 2026-09-25 that commands other than
record views print their JSON result.
**Design:** `docs/specs/2026-09-29-terminal-output-design.md`.

## Context

Record views (`list`, `status`, `engine list`, `engine detect`, `config show`)
printed tables, but every other command printed its JSON result on one line,
and roles mixed JSON banners, JSON events and text notices. `deploy --wait`
printed a 2 KB record; notices appeared inside the JSON and again on stderr.
The audience reads mllm in a terminal first.

## Decision

- `--format text|json` (`--json` short). Commands print text unless JSON is
  asked for, with no terminal detection.
- Single results print a summary line and aligned key-value details; lists
  print tables; `inspect` prints the full record in the same detail style.
- Roles print text when stderr is a terminal and JSON otherwise (journal,
  files, pipes); an explicit `--format` wins. Services therefore log JSON.
- `--output json` still means JSON, and `table` stays a hidden synonym of
  `text`.
- JSON output keeps its fields and bytes; role JSON events keep their fields.
- Role output goes through one sink in `mllm-domain`; library code does not
  print.

## Consequences

Scripts and the live harness pass `--json` for command results; role logs
redirected to files stay JSON without a flag. Guides show text output taken
from real runs. A new command gets the generic view until it has its own,
and never prints raw JSON in text mode.
