# Terminal output by default, JSON on request

Status: approved design, 2026-09-29 (owner). Supersedes the 2026-09-25 owner
decision that commands other than record views print their JSON result
(`crates/mllm-cli/src/main.rs`, `emit`). Recorded as ADR 0021.

## Goal

Every mllm command and role prints output a person can read in a terminal:
a table for lists of records, and a summary line with key-value details for
a single result. JSON is printed only when asked for, with one exception:
long-running roles write JSON when their output does not go to a terminal,
so the journal and log files stay machine-readable.

Success means: no default command output and no role output on a terminal is
a raw JSON line; one consistent style; `--json` output keeps its current
bytes; the guides show real output exactly; all local checks pass; a live run
shows text in a terminal and JSON in the journal and harness logs.

## 1. Format rule

- One option, `--format text|json`, with `--json` as short for
  `--format json`. `text` replaces the current `table` value; `table` stays
  accepted as a hidden synonym of `text`. The existing `--output json`
  synonym for JSON results is unchanged.
- **Commands** (everything except the three role starts; `start deployment`
  is a command): text unless the option asks for JSON. There is no terminal
  detection, so `mllm list deployments | grep ready` keeps reading the table.
- **Roles** (`start server`, `start host`, `start standalone`): with no
  option, text when stderr is a terminal and JSON otherwise (the journal, a
  redirect to a file, a pipe). An explicit `--format` always wins. The
  systemd units are unchanged and therefore log JSON.
- `--format` remains a per-command option, not a setting: it has no variable
  or YAML form (`docs/operations/configuration.md`, "Command options that
  are not settings").

### Streams

The result, or a role's banner, goes to stdout. Notices, warnings, progress
lines (`Waiting for …`, `Request identity: …`) and errors go to stderr.

- Text mode: every line is printed once. A notice that today appears inside
  the JSON result and again on stderr appears only on stderr.
- JSON mode, commands: stdout is exactly the current JSON result; errors are
  JSON on stderr, as now; text notices are not printed (they are fields of
  the result).
- JSON mode, roles: every line the role writes, on stdout and stderr, is one
  JSON object. Existing JSON events keep their exact fields.

## 2. Command views

### Style

- **Summary line**: one sentence naming what happened, with the name the
  user typed. Past tense when the work finished (`Parked my-model`);
  "requested" when the command returns before it finishes
  (`Park requested for my-model`).
- **Details**: a blank line, then key-value lines indented two spaces, keys
  in capitalized words, values aligned in one column.
- **Values** use the existing table helpers: GiB with one decimal, seconds,
  host names instead of host IDs where the inventory resolves them, `yes` and
  `no`. Null and empty fields are omitted.
- **IDs** appear only where the user needs them later: the operation ID of
  an asynchronous request, the host ID on join, the deployment ID on create.
- A view receives the parsed command as well as the JSON result, so a
  receipt that holds only IDs can still name the deployment the user typed.

### Per command

| Command | Summary | Details |
|---|---|---|
| `deploy` (no `--wait`) | `Deployment <name> created (revision <n>)`, or `updated` for a new revision; `joined` results say `Deployment <name> revision <n> already accepted` | Operation; Checkpoint digest (`being measured` while pending) |
| `deploy --wait`, `start deployment --wait` | `Deployed <name>: <state>` | Revision; Hosts; Ready `<ready>/<desired>`; Startup (GiB); Context (tokens); Operation (`<kind> <state>`) |
| `start`, `park`, `stop`, `preinitialize` (no `--wait`) | `<Verb> requested for <name>`; `Joined the <verb> already in progress for <name>` when `joined` | Operation |
| the same with `--wait` | `Started <name>` / `Parked <name>` / `Stopped <name>` / `Preinitialized <name>`, followed by `: <state>` | Ready; Hosts; Operation |
| instance lifecycle (`--instance`) | as above, naming `instance <i> of <name>` | as above |
| `delete` | `Deleted <name>` (or `Delete requested for <name>`) | Operation, when present |
| `init server`, `init host` | `Wrote <config>` | State directory; Runtime directory (host) |
| `invite host` | `Invitation for <host> written to <file>` | a second line: `Keep it private; it can be used once.` |
| `join host` | `Joined the server` | Host ID |
| `validate config` | `<file> is a valid <kind> document` | Resolved against, when present |
| `engine add` | `Registered <profile> (<engine> <version>)` | Executable; Deep park; CUDA; Engines file (revision); Published `yes` or `when mllm starts` |
| `engine remove` | `Removed <profile>` | Engines file (revision); Published |
| `drain` | `Drained <host>` | Engines stopped; Deployments kept |
| `revoke` | `Revoked <host>` | the recovery commands, as today's message gives them |
| `prune` | `Removed <n> unused model copies (<GiB>)` or `Nothing to remove` | one line per removed copy |
| `doctor` | the existing not-available error | — |
| `config show`, `list`, `status`, `engine list`, `engine detect` | the tables they print today | — |

The exact wording above is normative for the summary lines; detail keys may
be refined during implementation but must be listed in the view's unit test.

### `inspect` and the fallback

`inspect host|deployment` prints the whole record in the detail style:
scalars as key-value lines, nested objects as an indented section headed by
the capitalized key, arrays of scalars as a comma-separated value, arrays of
objects as numbered blocks (`Instance 0`, `Instance 1`). No field is dropped.

Any result that has no dedicated view prints `<command> done` followed by the
same generic rendering. A unit test enumerates every `Command` variant and
fails if one falls through to the fallback without being listed as intended,
and a test asserts that no text-mode output begins with `{`.

### Stderr lines kept

`Request identity: <id> (reuse --request-id <id> to recover this command)` is
still printed before the work starts, because it must survive a killed
terminal for recovery (SPEC §6.4 idempotency keys). `Waiting for …` progress
lines are unchanged.

## 3. Role log lines

### One sink

A small module in `mllm-domain` (the lowest crate that `mllm-cli`,
`mllm-controller`, `mllm-agent`, `mllm-router`, `mllm-adapters` and
`mllm-launchers` share) owns role output:

- `log_event(value: serde_json::Value)` for structured events;
- `log_notice(level, text)` for notices and warnings.

It holds the mode (text or JSON, default JSON so library tests and embedded
use are unchanged) and a text formatter installed by `mllm-cli` at role
start. Library code never prints directly; every current `eprintln!` and
`println!` in those crates outside tests goes through the sink.

### Text mode

- **Banner**, on stdout when the role is ready:

  ```
  mllm 0.1.0 standalone ready

    Inference     0.0.0.0:8443 (API key required)
    Management    127.0.0.1:7443
    State         ~/.local/state/mllm
    Credentials   ~/.local/state/mllm/identity/credentials
  ```

  The server adds its bootstrap and control listeners; a host adds its
  ingress and host ID.
- **Events**, one line each, prefixed with local time `HH:MM:SS`:
  `15:04:11 switch 01M3QRK1AP7M: planned; instance 0 wakes on gpu-box after releasing 1 instance(s)`,
  `15:04:13 switch 01M3QRK1AP7M: completed; the waiting instance is READY and its dispatch is open`,
  `15:04:14 request 01M3QRCJJ0MX -> gpu-box instance 0` (from `router_selection`; events carry
  deployment IDs, shown by their first 12 characters).
- **Notices**: `notice: …`, `warning: …`, as today.
- **Shutdown**: `mllm standalone stopped: drained, 0 requests in flight;
  engines kept running`.
- **Fallback**: an event without a formatter prints `HH:MM:SS <event>`
  followed by its scalar fields as `key=value`; never raw JSON.

### JSON mode

- Every line is one JSON object; no timestamp is added (the journal stamps
  lines).
- Existing JSON events keep their exact fields, so
  `scripts/live/matrix/selections.py` and `discrete_gpu.sh` keep working.
- Banners and shutdown summaries are JSON objects. The server banner keeps
  its current fields; the host and standalone banners gain the same shape
  (`role`, listeners, `state_dir`, credentials path).
- Notices become `{"level":"notice"|"warning","message":"…"}`.

Engine output is unchanged: it stays in `<state>/logs/`, and reaches the
terminal only with `--debug-engine-logs`.

## 4. Records, documentation and tests

### Records

- ADR 0021 records the decision and the superseded 2026-09-25 rule.
- The status runbook gets an entry.

### Documentation

- Every example of command or role output in `README.md` and `docs/guide/`
  is replaced with real output from a run on the laptop, per the rule that
  examples match real output exactly. The site builds from these guides;
  `gen-hero` runs the CLI and follows automatically.
- `docs/operations/configuration.md`: `--format text|json` and the role
  rule.
- `docs/operations/install.md`: services log JSON lines; read them with
  `journalctl -u mllm-host -o cat | jq`, or set `--format text` in a unit
  drop-in for text.
- Release notes: one line on the output behaviour.

### Tests

- A unit test per command view and per role event formatter: a JSON fixture
  in, the exact text out.
- The coverage test over `Command` variants and the no-`{` test (section 2).
- Format rule: a command prints text when its stdout is piped; a role prints
  JSON when piped (every existing role test runs this way); a role prints
  text with `--format text`; the terminal check is a unit test of the
  function that decides.
- Existing tests that parse command stdout as JSON pass `--json`; tests that
  match text banners match the JSON banner fields instead.
- Every command the live harness runs passes `--json`; any call that does not
  gets it.

### Verification before 0.1.0

The full local check set, then a live run: standalone on the laptop and a
server with hosts A and B, checking text in a terminal and JSON in the
journal-style redirected logs and in the harness. Then the 0.1.0 rebuild,
privacy scan, live check and draft already planned. CPU and Fake-engine
tests are not qualification.

## Out of scope

Colour, pagers, progress bars, localisation, and changing any JSON field.
