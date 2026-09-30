# Terminal Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every mllm command and role prints terminal-friendly text by default; JSON only on request, and role output that does not go to a terminal is JSON.

**Architecture:** A detail renderer and per-command views in `mllm-cli` turn each command's existing JSON result into a summary line plus key-value details; `main.rs` prints the view in text mode and today's JSON bytes in JSON mode. Role output goes through one sink in `mllm-domain` that holds the mode (text or JSON) and a text formatter that `mllm-cli` installs at role start; library crates call the sink instead of printing.

**Tech Stack:** Rust 2021, serde_json, clap, std `IsTerminal`; existing `table.rs` helpers.

**Spec:** `docs/specs/2026-09-29-terminal-output-design.md`

## Global Constraints

- `--format text|json`; `--json` is `--format json`; `table` is a hidden synonym of `text`; `--output json` still means JSON results.
- Commands: text unless JSON is asked for; no terminal detection.
- Roles (`start server|host|standalone`): no option means text when stderr is a terminal, JSON otherwise; an explicit option wins.
- JSON mode for commands prints exactly today's bytes on stdout. Existing JSON event fields never change.
- No text-mode output line begins with `{`.
- `Request identity: …` and `Waiting for …` lines stay on stderr as today.
- `--format` is a per-command option: no variable or YAML form.
- Code cites its requirement (`// ADR 0021: …`); tests carry their T-ID (`// T02` for CLI/role behaviour, `// T03` for configuration).
- Prose in docs and commit messages is normal English; no AI attribution, machine names, IPs or home paths in anything committed.
- CPU and Fake-engine tests are not qualification; say so in status claims.

## Review Focus

1. **A command piped into another program** (`mllm list deployments | grep ready`, `mllm deploy … > out.txt`) must print text, not JSON — pinned in Task 5.
2. **A role started by hand in a terminal but with stderr redirected** (`mllm start standalone 2>err.log`) follows stderr: JSON; with `--format text` it is text — pinned in Task 7.
3. **A result field that is absent or null** (receipt without `revision`, deployment without instances) prints no `null`/`-` noise and never panics — pinned in Task 4.
4. **A string value with a newline or control character** (an engine error message) must not break the aligned layout — pinned in Task 3.
5. **Scripts using `--json`** get byte-identical output to before — pinned in Task 5.

---

## File Structure

- Create `crates/mllm-domain/src/role_log.rs` — the role output sink (mode, formatter, `event`, `notice`, `line`).
- Create `crates/mllm-cli/src/detail.rs` — the detail view type and the generic record renderer.
- Create `crates/mllm-cli/src/views.rs` — one view per command result; `render(command, value, context)`.
- Create `crates/mllm-cli/src/role_text.rs` — the text formatter for role events, banners and shutdown summaries; `install(mode)`.
- Modify `crates/mllm-cli/src/output.rs` — `OutputFormat::resolve`.
- Modify `crates/mllm-cli/src/grammar.rs` — `--format` values.
- Modify `crates/mllm-cli/src/main.rs` — one `emit` for every command; role mode set before any role output.
- Modify `crates/mllm-cli/src/table.rs` — make `text`, `ready`, `instance_hosts`, `operation` `pub(crate)`.
- Modify the role and library print sites listed in Task 8.
- Modify tests under `crates/mllm-cli/tests/` that parse stdout as JSON (Task 6) and role banner matchers (Task 7).
- Modify `scripts/live/matrix/discrete_gpu.sh` (Task 9).
- Create `docs/design/adr/0021-terminal-output.md`; modify the spec, `docs/operations/configuration.md`, `docs/operations/install.md`, `docs/operations/release-notes-0.1.0.md`, `README.md`, `docs/guide/*.md`, `docs/runbooks/f2-current-status.md` (Tasks 1, 10, 11).

---

### Task 1: Decision record and spec corrections

**Files:**
- Create: `docs/design/adr/0021-terminal-output.md`
- Modify: `docs/specs/2026-09-29-terminal-output-design.md`

**Interfaces:** none (documents only).

- [ ] **Step 1: Correct two spec rows to what the results carry**

In `docs/specs/2026-09-29-terminal-output-design.md`, section 2 table, replace the `join host` row with:

```markdown
| `join host` | `Joined the server` | Host ID |
```

In section 3, replace the two switch examples with:

```markdown
  `15:04:11 switch 01M3QRK1AP7M: planned; instance 0 wakes on gpu-box after releasing 1 instance(s)`,
  `15:04:13 switch 01M3QRK1AP7M: completed; the waiting instance is READY and its dispatch is open`,
  `15:04:14 request 01M3QRCJJ0MX -> gpu-box instance 0` (from `router_selection`; events carry
  deployment IDs, shown by their first 12 characters).
```

- [ ] **Step 2: Write ADR 0021**

Create `docs/design/adr/0021-terminal-output.md`:

```markdown
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
- JSON output keeps its fields and bytes; role JSON events keep their fields.
- Role output goes through one sink in `mllm-domain`; library code does not
  print.

## Consequences

Scripts and the live harness pass `--json` for command results; role logs
redirected to files stay JSON without a flag. Guides show text output taken
from real runs. A new command gets the generic view until it has its own,
and never prints raw JSON in text mode.
```

- [ ] **Step 3: Commit**

```bash
git add docs/design/adr/0021-terminal-output.md docs/specs/2026-09-29-terminal-output-design.md
git commit -m "docs: record ADR 0021, terminal output by default"
```

---

### Task 2: Role output sink in mllm-domain

**Files:**
- Create: `crates/mllm-domain/src/role_log.rs`
- Modify: `crates/mllm-domain/src/lib.rs` (add `pub mod role_log;`)
- Modify: `crates/mllm-domain/Cargo.toml` (add `serde_json = { workspace = true }` under `[dependencies]`; if the workspace has no `serde_json` entry, use `serde_json = "1"`)

**Interfaces:**
- Produces:
  - `pub enum Mode { Json, Text }`
  - `pub enum Level { Notice, Warning }`
  - `pub type Formatter = fn(&serde_json::Value) -> Option<String>;`
  - `pub fn set_mode(mode: Mode)`; `pub fn mode() -> Mode` (default `Json`)
  - `pub fn set_formatter(formatter: Formatter)`
  - `pub fn event(value: serde_json::Value)` — a structured event, to stderr
  - `pub fn notice(level: Level, message: &str)` — a notice or warning, to stderr
  - `pub fn render_event(value: &serde_json::Value) -> String` and `pub fn render_notice(level: Level, message: &str) -> String` — the exact line `event`/`notice` would print (for tests)

- [ ] **Step 1: Write the failing tests**

Append to `crates/mllm-domain/src/role_log.rs` (create the file with only this test module first):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // T02 (ADR 0021): JSON mode prints the event unchanged; notices become objects.
    #[test]
    fn json_mode_keeps_events_and_wraps_notices() {
        let event = json!({"event": "switch", "phase": "Planned"});
        assert_eq!(render_event_in(Mode::Json, None, &event), event.to_string());
        assert_eq!(
            render_notice_in(Mode::Json, Level::Warning, "disk low"),
            json!({"level": "warning", "message": "disk low"}).to_string()
        );
    }

    // T02 (ADR 0021): text mode uses the formatter, else a key=value fallback,
    // never raw JSON.
    #[test]
    fn text_mode_formats_or_falls_back() {
        fn formatter(value: &serde_json::Value) -> Option<String> {
            (value["event"] == "known").then(|| "known event".to_owned())
        }
        let known = json!({"event": "known"});
        assert_eq!(render_event_in(Mode::Text, Some(formatter), &known), "known event");
        let other = json!({"event": "other", "stage": "wake", "n": 2, "nested": {"a": 1}});
        let line = render_event_in(Mode::Text, Some(formatter), &other);
        assert!(line.ends_with("other n=2 stage=wake"), "{line}");
        assert!(!line.starts_with('{'), "{line}");
        assert_eq!(render_notice_in(Mode::Text, Level::Warning, "disk low"), "warning: disk low");
        assert_eq!(render_notice_in(Mode::Text, Level::Notice, "hello"), "notice: hello");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mllm-domain role_log`
Expected: FAIL to compile (`render_event_in` not found).

- [ ] **Step 3: Implement**

Put above the test module in `crates/mllm-domain/src/role_log.rs`:

```rust
//! ADR 0021: the one place role output is written. Library crates call
//! [`event`] and [`notice`] instead of printing; the CLI sets the mode at role
//! start (text on a terminal, JSON otherwise) and installs the text formatter.
//! The default is JSON, so library tests and embedded use see today's lines.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Json,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Notice,
    Warning,
}

/// Text for one structured event, or `None` to use the generic fallback.
pub type Formatter = fn(&Value) -> Option<String>;

static MODE: AtomicU8 = AtomicU8::new(0);
static FORMATTER: OnceLock<Formatter> = OnceLock::new();

pub fn set_mode(mode: Mode) {
    MODE.store(matches!(mode, Mode::Text) as u8, Ordering::Relaxed);
}

pub fn mode() -> Mode {
    if MODE.load(Ordering::Relaxed) == 1 {
        Mode::Text
    } else {
        Mode::Json
    }
}

/// The first installed formatter wins; a second call is ignored.
pub fn set_formatter(formatter: Formatter) {
    let _ = FORMATTER.set(formatter);
}

/// Write one structured event to stderr in the current mode.
pub fn event(value: Value) {
    eprintln!("{}", render_event(&value));
}

/// Write one notice or warning to stderr in the current mode.
pub fn notice(level: Level, message: &str) {
    eprintln!("{}", render_notice(level, message));
}

pub fn render_event(value: &Value) -> String {
    render_event_in(mode(), FORMATTER.get().copied(), value)
}

pub fn render_notice(level: Level, message: &str) -> String {
    render_notice_in(mode(), level, message)
}

fn render_event_in(mode: Mode, formatter: Option<Formatter>, value: &Value) -> String {
    match mode {
        Mode::Json => value.to_string(),
        Mode::Text => formatter
            .and_then(|f| f(value))
            .unwrap_or_else(|| fallback(value)),
    }
}

fn render_notice_in(mode: Mode, level: Level, message: &str) -> String {
    let name = match level {
        Level::Notice => "notice",
        Level::Warning => "warning",
    };
    match mode {
        Mode::Json => serde_json::json!({"level": name, "message": message}).to_string(),
        Mode::Text => format!("{name}: {message}"),
    }
}

/// `<event> key=value …` over the scalar fields, sorted by key; nested
/// values are left out (they stay in JSON mode).
fn fallback(value: &Value) -> String {
    let name = value["event"].as_str().unwrap_or("event");
    let mut out = name.to_owned();
    if let Value::Object(fields) = value {
        let mut keys: Vec<&String> = fields.keys().filter(|k| k.as_str() != "event").collect();
        keys.sort();
        for key in keys {
            let text = match &fields[key] {
                Value::String(s) => s.replace(char::is_control, " "),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            out.push_str(&format!(" {key}={text}"));
        }
    }
    out
}
```

Add `pub mod role_log;` to `crates/mllm-domain/src/lib.rs` and the `serde_json` dependency to its `Cargo.toml`.

Note: the timestamp prefix for text events is added by the CLI formatter (Task 7), so this module stays deterministic.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-domain role_log`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-domain
git commit -m "feat: add the role output sink to mllm-domain"
```

---

### Task 3: Detail renderer

**Files:**
- Create: `crates/mllm-cli/src/detail.rs`
- Modify: `crates/mllm-cli/src/lib.rs` (add `pub mod detail;`)

**Interfaces:**
- Produces:
  - `pub struct Detail { pub summary: String, pub rows: Vec<(String, String)>, pub notes: Vec<String> }`
  - `impl Detail { pub fn new(summary: impl Into<String>) -> Self; pub fn row(self, key: &str, value: impl Into<String>) -> Self; pub fn row_opt(self, key: &str, value: Option<String>) -> Self; pub fn note(self, line: impl Into<String>) -> Self; pub fn render(&self) -> String }`
  - `pub fn record(summary: &str, value: &serde_json::Value) -> String` — the generic full-record rendering used by `inspect` and the fallback
  - `pub fn one_line(text: &str) -> String` — control characters to spaces

- [ ] **Step 1: Write the failing tests**

Create `crates/mllm-cli/src/detail.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // T02 (ADR 0021): summary, blank line, aligned two-space-indented rows,
    // then notes; empty values are left out.
    #[test]
    fn detail_aligns_rows_and_skips_empty_values() {
        let text = Detail::new("Deployed my-model: ready")
            .row("Revision", "1")
            .row("Hosts", "gpu-box")
            .row_opt("Context", None)
            .row("Operation", "")
            .note("Keep it private.")
            .render();
        assert_eq!(
            text,
            "Deployed my-model: ready\n\n  Revision   1\n  Hosts      gpu-box\n\nKeep it private.\n"
        );
    }

    // Review focus 4: a multi-line value cannot break the layout.
    #[test]
    fn control_characters_become_spaces() {
        let text = Detail::new("Failed").row("Error", "line one\nline two").render();
        assert_eq!(text, "Failed\n\n  Error   line one line two\n");
    }

    // T02 (ADR 0021): the generic record keeps every field: nested objects
    // become sections, object arrays numbered blocks, scalar arrays a list.
    #[test]
    fn record_renders_every_field() {
        let value = json!({
            "name": "my-model",
            "ready_instances": 1,
            "routes": ["my-model", "alias"],
            "timeouts": {"wake_ms": 105000, "provenance": {"wake": "derived"}},
            "instances": [{"index": 0, "host_id": "h1"}],
            "legacy_route": null
        });
        assert_eq!(
            record("Deployment my-model", &value),
            "Deployment my-model\n\n\
             \x20 Instances\n\
             \x20   Instance 0\n\
             \x20     Host ID   h1\n\
             \x20     Index     0\n\
             \x20 Name              my-model\n\
             \x20 Ready Instances   1\n\
             \x20 Routes            my-model, alias\n\
             \x20 Timeouts\n\
             \x20   Provenance\n\
             \x20     Wake   derived\n\
             \x20   Wake Ms   105000\n"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mllm-cli --lib detail`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

Put above the tests in `crates/mllm-cli/src/detail.rs`:

```rust
//! ADR 0021: the text form of a single result. A summary line, a blank line,
//! then key-value rows indented two spaces with values aligned in one column,
//! then optional notes. `record` renders any JSON value in the same style and
//! is what `inspect` and commands without their own view print.

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub summary: String,
    pub rows: Vec<(String, String)>,
    pub notes: Vec<String>,
}

impl Detail {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: one_line(&summary.into()),
            ..Self::default()
        }
    }

    /// A row; an empty or `-` value is left out.
    pub fn row(mut self, key: &str, value: impl Into<String>) -> Self {
        let value = one_line(&value.into());
        if !value.is_empty() && value != "-" {
            self.rows.push((key.to_owned(), value));
        }
        self
    }

    pub fn row_opt(self, key: &str, value: Option<String>) -> Self {
        match value {
            Some(value) => self.row(key, value),
            None => self,
        }
    }

    pub fn note(mut self, line: impl Into<String>) -> Self {
        self.notes.push(one_line(&line.into()));
        self
    }

    pub fn render(&self) -> String {
        let mut out = format!("{}\n", self.summary);
        if !self.rows.is_empty() {
            out.push('\n');
            let width = self.rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
            for (key, value) in &self.rows {
                let pad = width - key.chars().count();
                out.push_str(&format!("  {key}{}   {value}\n", " ".repeat(pad)));
            }
        }
        if !self.notes.is_empty() {
            out.push('\n');
            for note in &self.notes {
                out.push_str(note);
                out.push('\n');
            }
        }
        out
    }
}

/// Control characters (a multi-line engine error) become spaces.
pub fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// `ready_instances` -> `Ready Instances`; a trailing `_id` reads `ID`.
fn label(key: &str) -> String {
    key.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "id" => "ID".to_owned(),
            _ => {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(one_line(s)),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
            let parts: Vec<String> = items.iter().filter_map(scalar).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

/// Every field of `value` under `summary`: scalars and scalar lists as rows
/// (aligned per level), objects as indented sections, object lists as
/// numbered blocks named after the singular of the key.
pub fn record(summary: &str, value: &Value) -> String {
    let mut out = format!("{}\n", one_line(summary));
    let mut body = String::new();
    fields(value, 1, &mut body);
    if !body.is_empty() {
        out.push('\n');
        out.push_str(&body);
    }
    out
}

fn fields(value: &Value, depth: usize, out: &mut String) {
    let Value::Object(map) = value else {
        if let Some(text) = scalar(value) {
            out.push_str(&format!("{}{text}\n", "  ".repeat(depth)));
        }
        return;
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let indent = "  ".repeat(depth);
    let width = keys
        .iter()
        .filter(|k| scalar(&map[k.as_str()]).is_some())
        .map(|k| label(k).chars().count())
        .max()
        .unwrap_or(0);
    for key in keys {
        let item = &map[key.as_str()];
        if let Some(text) = scalar(item) {
            let name = label(key);
            let pad = width - name.chars().count();
            out.push_str(&format!("{indent}{name}{}   {text}\n", " ".repeat(pad)));
        } else if item.is_object() {
            out.push_str(&format!("{indent}{}\n", label(key)));
            fields(item, depth + 1, out);
        } else if let Value::Array(items) = item {
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("{indent}{}\n", label(key)));
            let singular = label(key.strip_suffix('s').unwrap_or(key));
            for (i, entry) in items.iter().enumerate() {
                out.push_str(&format!("{indent}  {singular} {i}\n"));
                fields(entry, depth + 2, out);
            }
        }
    }
}
```

Add `pub mod detail;` to `crates/mllm-cli/src/lib.rs` (alphabetical, after `pub mod deployment_file;`).

Note on the expected test text: keys at one level sort alphabetically (`instances`, `legacy_route` (null, skipped), `name`, `ready_instances`, `routes`, `timeouts`), and the scalar-row width at that level is the longest scalar label (`Ready Instances`, 15). Inside `timeouts`, `Wake Ms` is the only scalar, width 7; `Provenance` is a section. Inside `Instance 0`, width is `Host ID` (7). The rule above is normative: if the expected string and the rule disagree only in alignment spaces, recount against the rule and correct whichever is wrong; any other difference means the implementation is wrong.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-cli --lib detail`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli/src/detail.rs crates/mllm-cli/src/lib.rs
git commit -m "feat: add the detail view renderer"
```

---

### Task 4: Command views

**Files:**
- Create: `crates/mllm-cli/src/views.rs`
- Modify: `crates/mllm-cli/src/lib.rs` (add `pub mod views;`)
- Modify: `crates/mllm-cli/src/table.rs` (make `text`, `ready`, `instance_hosts`, `operation` `pub(crate)`)

**Interfaces:**
- Consumes: `detail::{Detail, record}` (Task 3); `table::{self, HostNames, View, gib, seconds, host_label}` and the four helpers made `pub(crate)`.
- Produces:
  - `pub struct Context<'a> { pub names: &'a HostNames, pub deployment_name: Option<String> }`
  - `pub fn render(command: &Command, value: &serde_json::Value, context: &Context) -> String` — the text of any command result; tables for record views
  - `pub fn has_view(command: &Command) -> bool` — false only for commands that use the generic fallback on purpose

- [ ] **Step 1: Write the failing tests**

Create `crates/mllm-cli/src/views.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{InitTarget, LifecycleAction};
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx(name: Option<&str>) -> (HostNames, Option<String>) {
        let mut names = HostNames::new();
        names.insert("h1".into(), "gpu-box".into());
        (names, name.map(str::to_owned))
    }

    fn run(command: Command, value: serde_json::Value, name: Option<&str>) -> String {
        let (names, deployment_name) = ctx(name);
        render(&command, &value, &Context { names: &names, deployment_name })
    }

    fn deploy(wait: bool) -> Command {
        Command::Deploy { file: Some(PathBuf::from("my-model.yaml")), activate: true, wait, revision: None, hf_endpoint: None }
    }

    fn deployment() -> serde_json::Value {
        json!({
            "name": "my-model", "observed_state": "ready", "revision": "1",
            "ready_instances": 1, "desired_instances": 1,
            "startup": {"bytes": 18_467_520_512i64}, "context": {"tokens": 26752},
            "latest_operation": {"kind": "initialize", "state": "succeeded"},
            "instances": [{"host_id": "h1", "index": 0}]
        })
    }

    // T02 (ADR 0021)
    #[test]
    fn deploy_without_wait_names_the_deployment_and_operation() {
        let receipt = json!({"api_version": "1", "deployment_id": "01D", "joined": false,
            "operation_id": "01OP", "revision": "1", "checkpoint_digest": "pending",
            "notice": "the checkpoint digest of my-model is being measured"});
        assert_eq!(
            run(deploy(false), receipt, Some("my-model")),
            "Deployment my-model created (revision 1)\n\n  Deployment ID       01D\n  Operation           01OP\n  Checkpoint digest   being measured\n"
        );
    }

    // T02 (ADR 0021)
    #[test]
    fn deploy_with_wait_prints_the_state_not_the_record() {
        let value = json!({"deployment": deployment(), "receipt": {"operation_id": "01OP", "revision": "1"}});
        assert_eq!(
            run(deploy(true), value, Some("my-model")),
            "Deployed my-model: ready\n\n  Revision    1\n  Hosts       gpu-box\n  Ready       1/1\n  Startup     17.2 GiB\n  Context     26752 tokens\n  Operation   initialize succeeded\n"
        );
    }

    // T02 (ADR 0021): asynchronous lifecycle requests, joined or new.
    #[test]
    fn lifecycle_requests_and_joins() {
        let park = Command::Lifecycle { action: LifecycleAction::Park, deployment: "my-model".into() };
        assert_eq!(
            run(park.clone(), json!({"operation_id": "01OP", "joined": false}), None),
            "Park requested for my-model\n\n  Operation   01OP\n"
        );
        assert_eq!(
            run(park, json!({"operation_id": "01OP", "joined": true}), None),
            "Joined the park already in progress for my-model\n\n  Operation   01OP\n"
        );
    }

    // T02 (ADR 0021): with --wait the result holds the deployment.
    #[test]
    fn lifecycle_with_wait_reports_the_outcome() {
        let stop = Command::InstanceLifecycle { action: LifecycleAction::Stop, deployment: "my-model".into(), instance: 0 };
        let mut d = deployment();
        d["observed_state"] = json!("stopped");
        assert_eq!(
            run(stop, json!({"deployment": d, "receipt": {"operation_id": "01OP"}}), None),
            "Stopped instance 0 of my-model: stopped\n\n  Ready       1/1\n  Hosts       gpu-box\n  Operation   initialize succeeded\n"
        );
    }

    // T02 T04 (ADR 0021)
    #[test]
    fn enrolment_commands() {
        assert_eq!(
            run(Command::Init(InitTarget::Host), json!({"config": "host.yaml", "initialized": true, "state_dir": "/s", "runtime_dir": "/s/runtime"}), None),
            "Wrote host.yaml\n\n  State directory     /s\n  Runtime directory   /s/runtime\n"
        );
        assert_eq!(
            run(Command::Invite { name: "gpu-box".into(), recover: false }, json!({"host_name": "gpu-box", "invitation_file": "gpu-box.join"}), None),
            "Invitation for gpu-box written to gpu-box.join\n\nKeep it private; it can be used once.\n"
        );
        assert_eq!(
            run(Command::Join { join_file: "gpu-box.join".into(), recover: false }, json!({"enrolled": true, "host_id": "01H"}), None),
            "Joined the server\n\n  Host ID   01H\n"
        );
    }

    // T02 (ADR 0018, ADR 0021)
    #[test]
    fn engine_add_and_remove() {
        let add = Command::EngineAdd { path: None, name: None, deep_park: None, drift: crate::grammar::DriftChoice::default(), args: vec![] };
        let value = json!({"profile": "vllm", "engine": "vllm", "version": "0.29.0", "executable": "/v/bin/vllm",
            "deep_park": "enabled", "cuda_home": "/usr/local/cuda", "engines_file": "/c/engines.yaml",
            "revision": 1, "published": "role_not_running"});
        assert_eq!(
            run(add, value, None),
            "Registered vllm (vllm 0.29.0)\n\n  Executable     /v/bin/vllm\n  Deep park      enabled\n  CUDA           /usr/local/cuda\n  Engines file   /c/engines.yaml (revision 1)\n  Published      when mllm starts\n"
        );
        let remove = Command::EngineRemove { name: "vllm".into(), drain: false };
        assert_eq!(
            run(remove, json!({"engines_file": "/c/engines.yaml", "published": "published", "removed": "vllm", "revision": 3}), None),
            "Removed vllm\n\n  Engines file   /c/engines.yaml (revision 3)\n  Published      yes\n"
        );
    }

    // Review focus 3: absent fields print nothing and never panic.
    #[test]
    fn absent_fields_are_left_out() {
        let park = Command::Lifecycle { action: LifecycleAction::Start, deployment: "m".into() };
        assert_eq!(run(park, json!({}), None), "Start requested for m\n");
        assert_eq!(run(deploy(false), json!({}), None), "Deployment created\n");
    }

    // T02 (ADR 0021): inspect keeps every field; no text output starts with `{`.
    #[test]
    fn inspect_and_fallback_never_print_json() {
        let inspect = Command::Inspect { resource: crate::grammar::Resource::Deployment, id: Some("my-model".into()), effective: false };
        let text = run(inspect, deployment(), None);
        assert!(text.starts_with("Deployment my-model\n\n"), "{text}");
        assert!(
            text.lines().any(|l| l.trim_start().starts_with("Observed State") && l.ends_with(" ready")),
            "{text}"
        );
        let validate = Command::Validate { file: "d.yaml".into(), host: None, sets: vec![] };
        assert_eq!(
            run(validate, json!({"file": "d.yaml", "kind": "deployment", "valid": true, "resolved_against": null}), None),
            "d.yaml is a valid deployment document\n"
        );
    }

    // T02 (ADR 0021): every command has a view, or is listed as using the
    // generic one on purpose.
    #[test]
    fn every_command_has_a_view() {
        use crate::grammar::{ListResource, Role};
        let all = vec![
            Command::Start(Role::Server), Command::Init(InitTarget::Server),
            Command::Invite { name: "h".into(), recover: false },
            Command::Join { join_file: "j".into(), recover: false },
            Command::List { resource: ListResource::Hosts },
            Command::Inspect { resource: crate::grammar::Resource::Host, id: None, effective: false },
            Command::Doctor { host: "h".into() }, deploy(false),
            Command::Status { deployment: "d".into(), watch: false },
            Command::Lifecycle { action: LifecycleAction::Park, deployment: "d".into() },
            Command::InstanceLifecycle { action: LifecycleAction::Park, deployment: "d".into(), instance: 0 },
            Command::Delete { deployment: "d".into(), stop: true },
            Command::Validate { file: "f".into(), host: None, sets: vec![] },
            Command::ConfigShow { role: None, sets: vec![] },
            Command::Drain { host: None, wait: false }, Command::Revoke { host: "h".into() },
            Command::PruneSources { host_config: "h.yaml".into(), apply: false, referenced_file: None },
            Command::EngineDetect { paths: vec![] },
            Command::EngineAdd { path: None, name: None, deep_park: None, drift: crate::grammar::DriftChoice::default(), args: vec![] },
            Command::EngineList, Command::EngineRemove { name: "v".into(), drain: false },
        ];
        // `doctor` is refused before it has a result.
        let generic = ["Doctor"];
        for command in all {
            let name = format!("{command:?}");
            let name = name.split([' ', '(', '{']).next().unwrap();
            assert_eq!(has_view(&command), !generic.contains(&name), "{name}");
        }
    }

    // T02 (ADR 0021): drain, revoke and prune summaries.
    #[test]
    fn host_maintenance_commands() {
        assert_eq!(
            run(Command::Revoke { host: "gpu-box".into() }, json!({"host_id": "01H", "name": "gpu-box", "revoked": true, "newly_revoked": true, "engines": "retained"}), None),
            "Revoked gpu-box\n\n  Host ID   01H\n  Engines   retained\n\nTo bring it back: mllm invite host gpu-box --recover --output FILE, then mllm join host --join-file FILE --recover on the host.\n"
        );
        assert_eq!(
            run(Command::Drain { host: Some("gpu-box".into()), wait: false }, json!({"host": "gpu-box", "host_state": "offline", "drained": false, "stops": "pending", "operations": [{"deployment_id": "01D", "instance": 0, "operation_id": "01OP"}]}), None),
            "Drain requested for gpu-box\n\n  Host state   offline\n  Stops        pending (1)\n"
        );
        assert_eq!(
            run(Command::PruneSources { host_config: "h.yaml".into(), apply: false, referenced_file: None }, json!({"model_store": "/m", "applied": false, "removed": [{"key": "sources/hf/a", "bytes": 1073741824}], "removed_bytes": 1073741824, "kept": [], "skipped": []}), None),
            "Would remove 1 unused model copy (1.0 GiB); run again with --apply to remove it\n\n  Model store   /m\n\n  sources/hf/a   1.0 GiB\n"
        );
    }
}
```

`DriftChoice` derives `Default` (`Warn`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mllm-cli --lib views`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

In `crates/mllm-cli/src/table.rs`, change `fn text(`, `fn ready(`, `fn instance_hosts(`, `fn operation(` to `pub(crate) fn …`.

Put above the tests in `crates/mllm-cli/src/views.rs`:

```rust
//! ADR 0021: the text form of every command result, built only from the JSON
//! result the command already returns (so text never shows what JSON does
//! not carry) plus the parsed command for names the result lacks.

use serde_json::Value;

use crate::detail::{record, Detail};
use crate::grammar::{Command, InitTarget, LifecycleAction, Resource};
use crate::table::{self, gib, host_label, HostNames, View};

pub struct Context<'a> {
    pub names: &'a HostNames,
    /// The `name` of the deployment file `deploy` read, when known.
    pub deployment_name: Option<String>,
}

pub fn has_view(command: &Command) -> bool {
    !matches!(command, Command::Doctor { .. })
}

pub fn render(command: &Command, value: &Value, context: &Context) -> String {
    if let Some(view) = View::of(command) {
        return table::render(view, value, context.names);
    }
    match command {
        Command::Deploy { .. } => deploy(value, context),
        Command::Lifecycle { action, deployment } => lifecycle(*action, deployment, None, value, context),
        Command::InstanceLifecycle { action, deployment, instance } => {
            lifecycle(*action, deployment, Some(*instance), value, context)
        }
        Command::Delete { deployment, .. } => delete(deployment, value),
        Command::Init(target) => init(*target, value),
        Command::Invite { .. } => invite(value),
        Command::Join { .. } => Detail::new("Joined the server").row("Host ID", s(&value["host_id"])).render(),
        Command::Validate { .. } => validate(value),
        Command::EngineAdd { .. } => engine_add(value),
        Command::EngineRemove { .. } => engine_remove(value),
        Command::Drain { host, .. } => drain(host.as_deref(), value),
        Command::Revoke { host } => revoke(host, value),
        Command::PruneSources { .. } => prune(value),
        Command::Inspect { resource, id, .. } => {
            let kind = match resource {
                Resource::Host => "Host",
                Resource::Deployment => "Deployment",
                Resource::Config => "Configuration",
            };
            let name = value["name"].as_str().map(str::to_owned).or_else(|| id.clone()).unwrap_or_default();
            record(format!("{kind} {name}").trim_end(), value)
        }
        other => {
            let name = format!("{other:?}");
            let name = name.split([' ', '(', '{']).next().unwrap_or("command").to_lowercase();
            record(&format!("{name} done"), value)
        }
    }
}

/// A scalar as text; empty for null or absent.
fn s(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn opt(value: &Value) -> Option<String> {
    Some(s(value)).filter(|text| !text.is_empty())
}

fn verb(action: LifecycleAction) -> (&'static str, &'static str) {
    match action {
        LifecycleAction::Start => ("Start", "Started"),
        LifecycleAction::Park => ("Park", "Parked"),
        LifecycleAction::Stop => ("Stop", "Stopped"),
        LifecycleAction::Preinitialize => ("Preinitialize", "Preinitialized"),
    }
}

fn state_rows(detail: Detail, d: &Value, context: &Context) -> Detail {
    detail
        .row("Ready", table::ready(d))
        .row("Hosts", table::instance_hosts(d, context.names))
        .row("Operation", table::operation(&d["latest_operation"]))
}

fn deploy(value: &Value, context: &Context) -> String {
    let d = &value["deployment"];
    if d.is_object() {
        let name = opt(&d["name"]).unwrap_or_default();
        return Detail::new(format!("Deployed {name}: {}", s(&d["observed_state"])))
            .row("Revision", s(&d["revision"]))
            .row("Hosts", table::instance_hosts(d, context.names))
            .row("Ready", table::ready(d))
            .row_opt("Startup", d["startup"]["bytes"].as_i64().map(gib))
            .row_opt("Context", d["context"]["tokens"].as_i64().map(|t| format!("{t} tokens")))
            .row("Operation", table::operation(&d["latest_operation"]))
            .render();
    }
    let name = context.deployment_name.as_deref().map(|n| format!(" {n}")).unwrap_or_default();
    let summary = match (value["joined"].as_bool(), opt(&value["revision"])) {
        (Some(true), Some(rev)) => format!("Deployment{name} revision {rev} already accepted"),
        (_, Some(rev)) if rev != "1" => format!("Deployment{name} updated (revision {rev})"),
        (_, Some(rev)) => format!("Deployment{name} created (revision {rev})"),
        (_, None) => format!("Deployment{name} created"),
    };
    let digest = match value["checkpoint_digest"].as_str() {
        Some("pending") => Some("being measured".to_owned()),
        other => other.map(str::to_owned),
    };
    Detail::new(summary)
        .row("Deployment ID", s(&value["deployment_id"]))
        .row("Operation", s(&value["operation_id"]))
        .row_opt("Checkpoint digest", digest)
        .render()
}

fn lifecycle(action: LifecycleAction, deployment: &str, instance: Option<u32>, value: &Value, context: &Context) -> String {
    let (request, done) = verb(action);
    let subject = match instance {
        Some(i) => format!("instance {i} of {deployment}"),
        None => deployment.to_owned(),
    };
    let d = &value["deployment"];
    if d.is_object() {
        return state_rows(Detail::new(format!("{done} {subject}: {}", s(&d["observed_state"]))), d, context).render();
    }
    let operation = opt(&value["operation_id"]).or_else(|| opt(&value["receipt"]["operation_id"]));
    let summary = if value["joined"] == true {
        format!("Joined the {} already in progress for {subject}", request.to_lowercase())
    } else {
        format!("{request} requested for {subject}")
    };
    Detail::new(summary).row_opt("Operation", operation).render()
}

fn delete(deployment: &str, value: &Value) -> String {
    let done = value["deleted"] == true || value["state"] == "deleted";
    let summary = if done { format!("Deleted {deployment}") } else { format!("Delete requested for {deployment}") };
    Detail::new(summary).row_opt("Operation", opt(&value["operation_id"])).render()
}

fn init(target: InitTarget, value: &Value) -> String {
    let detail = Detail::new(format!("Wrote {}", s(&value["config"]))).row("State directory", s(&value["state_dir"]));
    match target {
        InitTarget::Host => detail.row("Runtime directory", s(&value["runtime_dir"])),
        InitTarget::Server => detail,
    }
    .render()
}

fn invite(value: &Value) -> String {
    Detail::new(format!(
        "Invitation for {} written to {}",
        s(&value["host_name"]),
        s(&value["invitation_file"])
    ))
    .note("Keep it private; it can be used once.")
    .render()
}

fn validate(value: &Value) -> String {
    Detail::new(format!("{} is a valid {} document", s(&value["file"]), s(&value["kind"])))
        .row_opt("Resolved against", opt(&value["resolved_against"]))
        .render()
}

fn published(value: &Value) -> String {
    match value["published"].as_str() {
        Some("published") => "yes".into(),
        Some("role_not_running") => "when mllm starts".into(),
        Some(other) => other.replace('_', " "),
        None => String::new(),
    }
}

fn engines_file(value: &Value) -> String {
    match opt(&value["revision"]) {
        Some(rev) => format!("{} (revision {rev})", s(&value["engines_file"])),
        None => s(&value["engines_file"]),
    }
}

fn engine_add(value: &Value) -> String {
    Detail::new(format!("Registered {} ({} {})", s(&value["profile"]), s(&value["engine"]), s(&value["version"])))
        .row("Executable", s(&value["executable"]))
        .row("Deep park", s(&value["deep_park"]))
        .row("CUDA", s(&value["cuda_home"]))
        .row("Engines file", engines_file(value))
        .row("Published", published(value))
        .render()
}

fn engine_remove(value: &Value) -> String {
    Detail::new(format!("Removed {}", s(&value["removed"])))
        .row("Engines file", engines_file(value))
        .row("Published", published(value))
        .render()
}

fn drain(host: Option<&str>, value: &Value) -> String {
    let host = opt(&value["host"]).or(host.map(str::to_owned)).unwrap_or_else(|| "this machine".into());
    let stops = value["operations"].as_array().map_or(0, Vec::len);
    let summary = if value["drained"] == true { format!("Drained {host}") } else { format!("Drain requested for {host}") };
    Detail::new(summary)
        .row_opt("Host state", opt(&value["host_state"]))
        .row_opt("Stops", opt(&value["stops"]).map(|state| format!("{state} ({stops})")))
        .render()
}

fn revoke(host: &str, value: &Value) -> String {
    let name = opt(&value["name"]).unwrap_or_else(|| host.to_owned());
    Detail::new(format!("Revoked {name}"))
        .row("Host ID", s(&value["host_id"]))
        .row("Engines", s(&value["engines"]))
        .note(format!(
            "To bring it back: mllm invite host {name} --recover --output FILE, then mllm join host --join-file FILE --recover on the host."
        ))
        .render()
}

fn prune(value: &Value) -> String {
    let removed = value["removed"].as_array().cloned().unwrap_or_default();
    let bytes = value["removed_bytes"].as_i64().unwrap_or(0);
    let applied = value["applied"] == true;
    let copies = if removed.len() == 1 { "copy" } else { "copies" };
    let summary = match (removed.is_empty(), applied) {
        (true, _) => "Nothing to remove".to_owned(),
        (false, true) => format!("Removed {} unused model {copies} ({})", removed.len(), gib(bytes)),
        (false, false) => format!(
            "Would remove {} unused model {copies} ({}); run again with --apply to remove {}",
            removed.len(),
            gib(bytes),
            if removed.len() == 1 { "it" } else { "them" }
        ),
    };
    let mut out = Detail::new(summary).row("Model store", s(&value["model_store"])).render();
    if !removed.is_empty() {
        let rows: Vec<(String, String)> = removed
            .iter()
            .map(|r| (s(&r["key"]), r["bytes"].as_i64().map(gib).unwrap_or_default()))
            .collect();
        let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        out.push('\n');
        for (key, size) in rows {
            out.push_str(&format!("  {key}{}   {size}\n", " ".repeat(width - key.chars().count())));
        }
    }
    out
}

#[allow(dead_code)]
fn host(id: &str, context: &Context) -> String {
    host_label(id, context.names)
}
```

Remove the `#[allow(dead_code)] fn host` helper if nothing uses it after the tests pass.

Before running, check the `delete` result shape in `crates/mllm-cli/src/client.rs` (search `Command::Delete`): if the result carries a field other than `deleted`/`state` that says the deletion finished, use it in `delete()`, and add a test with that shape to `absent_fields_are_left_out`.

Add `pub mod views;` to `crates/mllm-cli/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-cli --lib views`
Expected: PASS (9 tests). Where an expected string differs only because the real result shape differs from the fixture (checked against `client.rs`, `drain.rs`, `revoke.rs`, `prune.rs`, `engine.rs`, `remote_roles.rs`), fix the fixture to the real shape, keeping the expected text's wording.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli/src
git commit -m "feat: add text views for every command result"
```

---

### Task 5: Format resolution and one emit path

**Files:**
- Modify: `crates/mllm-cli/src/output.rs` (add `OutputFormat::resolve`)
- Modify: `crates/mllm-cli/src/grammar.rs:289-292` (`--format` values and help)
- Modify: `crates/mllm-cli/src/main.rs` (all result printing through `emit`)
- Test: `crates/mllm-cli/tests/terminal_output.rs` (new)

**Interfaces:**
- Consumes: `views::{render, Context}` (Task 4).
- Produces:
  - `impl OutputFormat { pub fn resolve(format: Option<&str>, output: Option<&str>, role: bool, stderr_is_terminal: bool) -> OutputFormat }`
  - `fn emit(command: &Command, value: &Value, format: OutputFormat, context: &views::Context)` in `main.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module of `crates/mllm-cli/src/output.rs` (create one at the end of the file if absent):

```rust
#[cfg(test)]
mod format_tests {
    use super::OutputFormat;

    // T02 (ADR 0021): commands never detect; roles follow stderr unless told.
    #[test]
    fn resolve_follows_the_format_rule() {
        use OutputFormat::{Json, Text};
        assert_eq!(OutputFormat::resolve(None, None, false, false), Text);
        assert_eq!(OutputFormat::resolve(None, None, false, true), Text);
        assert_eq!(OutputFormat::resolve(Some("json"), None, false, true), Json);
        assert_eq!(OutputFormat::resolve(None, Some("json"), false, true), Json);
        assert_eq!(OutputFormat::resolve(Some("table"), None, false, false), Text);
        assert_eq!(OutputFormat::resolve(None, None, true, true), Text);
        assert_eq!(OutputFormat::resolve(None, None, true, false), Json);
        assert_eq!(OutputFormat::resolve(Some("text"), None, true, false), Text);
        assert_eq!(OutputFormat::resolve(Some("json"), None, true, true), Json);
    }
}
```

Create `crates/mllm-cli/tests/terminal_output.rs`:

```rust
//! ADR 0021: commands print text by default, even when piped, and JSON only
//! when asked; `--json` output is today's JSON result. CPU test; not
//! qualification.
mod support;

use std::os::unix::fs::PermissionsExt;

fn state() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    support::mllm().env("MLLM_STATE_DIR", dir.join("s")).args(args).output().unwrap()
}

// T02 T03 (ADR 0021): a piped command prints its text view; `--json` prints
// the JSON result, one object on one line.
#[test]
fn init_prints_text_when_piped_and_json_on_request() {
    let dir = state();
    let text = run(dir.path(), &["init", "host", "--output", dir.path().join("a.yaml").to_str().unwrap()]);
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(stdout.starts_with(&format!("Wrote {}\n", dir.path().join("a.yaml").display())), "{stdout}");
    assert!(!stdout.lines().any(|line| line.starts_with('{')), "{stdout}");

    let json = run(dir.path(), &["init", "host", "--output", dir.path().join("b.yaml").to_str().unwrap(), "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["initialized"], true);
    assert_eq!(String::from_utf8(json.stdout).unwrap().lines().count(), 1);
}

// T02 (ADR 0021): a notice prints once, on stderr, in text mode.
#[test]
fn validate_prints_a_sentence() {
    let dir = state();
    let file = dir.path().join("host.yaml");
    assert!(run(dir.path(), &["init", "host", "--output", file.to_str().unwrap()]).status.success());
    let out = run(dir.path(), &["validate", "config", "--file", file.to_str().unwrap()]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{} is a valid host document\n", file.display())
    );
}
```

If `init host` needs a GPU observation or other environment the test sandbox lacks, reuse the environment `crates/mllm-cli/tests/remote_roles.rs` sets for its `cli(&state, &["init", "host", …])` helper (read that helper and copy its `env` calls into `run`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-cli --lib format_tests && cargo test -p mllm-cli --test terminal_output`
Expected: FAIL (`resolve` missing; `init` prints JSON).

- [ ] **Step 3: Implement**

In `crates/mllm-cli/src/output.rs`, add to `impl OutputFormat`:

```rust
    /// ADR 0021: `--format` (or `--output json`) wins. Otherwise a command
    /// prints text, and a role prints text only when stderr is a terminal,
    /// so the journal, files and pipes get JSON lines.
    pub fn resolve(format: Option<&str>, output: Option<&str>, role: bool, stderr_is_terminal: bool) -> OutputFormat {
        match (format, output) {
            (Some("json"), _) => OutputFormat::Json,
            (Some(_), _) => OutputFormat::Text,
            (None, Some("json")) => OutputFormat::Json,
            _ if role && !stderr_is_terminal => OutputFormat::Json,
            _ => OutputFormat::Text,
        }
    }
```

In `crates/mllm-cli/src/grammar.rs`, replace the `format` argument (lines 289-292) with:

```rust
    /// How results print: `text` (the default: tables and summaries) or
    /// `json`. A role (`start server|host|standalone`) prints JSON lines when
    /// its output is not a terminal, unless this option says otherwise.
    #[arg(long, global = true, value_name = "FORMAT",
          value_parser = clap::builder::PossibleValuesParser::new([
              clap::builder::PossibleValue::new("text"),
              clap::builder::PossibleValue::new("json"),
              clap::builder::PossibleValue::new("table").hide(true),
          ]))]
    format: Option<String>,
```

and update the `Invocation::format` doc comment to "ADR 0021: `--format text|json` (`table` is a hidden synonym of `text`)".

In `crates/mllm-cli/src/main.rs`:

1. Replace the `format`/`json_records`/`view` computation (lines 23-37) with:

```rust
    // ADR 0021: one format rule for commands and roles.
    let role = matches!(invocation.command, Command::Start(Role::Server | Role::Host | Role::Standalone));
    let format = OutputFormat::resolve(
        invocation.format.as_deref(),
        invocation.output.as_deref(),
        role,
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    );
```

2. Replace `fn emit` with:

```rust
/// ADR 0021: text views by default, the JSON result unchanged on request.
fn emit(command: &Command, value: &serde_json::Value, format: OutputFormat, context: &mllm_cli::views::Context) {
    match format {
        OutputFormat::Json => println!("{value}"),
        OutputFormat::Text => print!("{}", mllm_cli::views::render(command, value, context)),
    }
}

/// The `name` a deployment file states, for the text of `deploy`.
fn deployment_name(command: &Command) -> Option<String> {
    let Command::Deploy { file: Some(file), .. } = command else { return None };
    let text = std::fs::read_to_string(file).ok()?;
    mllm_config::parse_document(&text).ok()?["name"].as_str().map(str::to_owned)
}
```

3. Every success arm that prints a result calls `emit(&invocation.command, &value, format, &context)`, where `context` is:

```rust
let context = mllm_cli::views::Context { names: &names, deployment_name: deployment_name(&invocation.command) };
```

with `names` the host names already computed for the client path (`needs_host_names`), else `HostNames::default()`. This replaces `println!("{value}")` in the drain, revoke, prune and validate arms, and `emit(&value, view, …)` in the remote-roles, engine and client arms. `config show` keeps its own branch but uses `format == OutputFormat::Json` instead of `json_records`.

4. Remove the two `if format == OutputFormat::Text { if let Some(notice) = value["notice"] … eprintln!(…) }` blocks' duplication: keep printing `value["notice"]` on stderr in text mode, and make the views never print `notice` (they do not). The notice therefore appears once in text mode (stderr) and once in JSON mode (inside the result).

5. The `table` import in `main.rs` and `View::of(...)` filtering become unused; remove them.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-cli --lib format_tests && cargo test -p mllm-cli --test terminal_output`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat: print command results as text unless JSON is asked for"
```

---

### Task 6: Tests that read command JSON ask for it

**Files:**
- Modify: the tests in `crates/mllm-cli/tests/` that parse a command's stdout as JSON. The current list: `minimal_deployment_cli.rs`, `local_role.rs`, `engine_exit.rs`, `wording_gate.rs`, `first_run.rs`, `initialize_timeout.rs`, `role_config_env.rs`, `two_host_instances.rs`, `switching_cli.rs`, `management_cli.rs`, `host_recovery.rs`, `test_isolation.rs`, `start_wait_replicas.rs`, `remote_roles.rs`, `engine_cli.rs`, `setting_overrides_cli.rs`, `role_shutdown.rs`, `startup_budget.rs`, `two_host_balance.rs`, `validate_config.rs`.

**Interfaces:** none.

- [ ] **Step 1: Run the CLI suite to list the failures Task 5 caused**

Run: `cargo test -p mllm-cli --tests --no-fail-fast 2>&1 | grep -E "^test .* FAILED|panicked at" | sort -u > /tmp/terminal-output-failures.txt; wc -l /tmp/terminal-output-failures.txt`
Expected: failures in the files above, each at a `serde_json::from_slice(&out.stdout)` (or `from_str`) on a command's output.

- [ ] **Step 2: Add `--json` where a test parses command stdout as JSON**

For each failing test, find the `args([...])` (or helper call) that produced the parsed stdout and append `"--json"` to that command's arguments. Do not add it to role starts (`start server|host|standalone`): their stdout is piped in tests, so they already print JSON. Where a helper builds many commands (for example a `cli(&state, &[…])` function), add `"--json"` inside the helper only if every caller parses JSON; otherwise add it per call.

Where a test asserts the table text of a record view (for example `management_cli.rs:75`, "--format table is the default"), keep it: record views are unchanged.

- [ ] **Step 3: Run the CLI suite**

Run: `cargo test -p mllm-cli --tests --no-fail-fast 2>&1 | grep -E "^test result|FAILED"`
Expected: every `test result: ok`, except role-banner tests fixed in Task 7 (`role_shutdown.rs` tests that match `starts_with("standalone ready")` or `starts_with("host ready")`, and `engine_exit.rs:420`). List any other remaining failure and fix it the same way.

- [ ] **Step 4: Commit**

```bash
git add crates/mllm-cli/tests
git commit -m "test: ask for JSON where tests parse command results"
```

---

### Task 7: Role text: banners, events, shutdown

**Files:**
- Create: `crates/mllm-cli/src/role_text.rs`
- Modify: `crates/mllm-cli/src/lib.rs` (add `pub mod role_text;`)
- Modify: `crates/mllm-cli/src/main.rs` (set the role mode before any role output; standalone banner and shutdown summary)
- Modify: `crates/mllm-cli/src/remote_roles.rs:760-767` (server banner), `:1116-1126` (host banner), and the server/host shutdown result (printed by `emit` in `main.rs`)
- Test: `crates/mllm-cli/tests/role_shutdown.rs` (banner matchers), `crates/mllm-cli/tests/engine_exit.rs:420`

**Interfaces:**
- Consumes: `mllm_domain::role_log::{set_mode, set_formatter, Mode, render_event}` (Task 2); `detail::Detail` (Task 3).
- Produces:
  - `pub fn install(format: OutputFormat)` — sets the sink mode and formatter
  - `pub fn format_event(value: &Value) -> Option<String>` — the installed formatter (adds `HH:MM:SS `)
  - `pub fn banner(value: &Value) -> String` — a role's ready banner in the current mode
  - `pub fn stopped(value: &Value) -> String` — a role's shutdown summary in the current mode
  - Banner JSON shape: `{"role": "server"|"host"|"standalone", "ready": true, "version", "state_dir", "credentials", "inference"?, "management"?, "bootstrap"?, "control"?, "ingress"?}` — the server banner keeps its current keys and adds only `ready` and `version`.

- [ ] **Step 1: Write the failing tests**

Create `crates/mllm-cli/src/role_text.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn without_time(line: &str) -> &str {
        line.get(9..).unwrap_or(line)
    }

    // T02 (ADR 0021)
    #[test]
    fn switch_and_router_events_read_as_sentences() {
        let switch = json!({"event": "switch", "phase": "Planned", "switch_id": "01M3QRK1AP7MFYHMZ3AG",
            "target": "01M3QRCJJ0MXR2F8ZAM3", "host": "gpu-box",
            "detail": "instance 0 wakes on gpu-box after releasing 1 instance(s)"});
        let line = format_event(&switch).unwrap();
        assert_eq!(&line[2..3], ":", "{line}");
        assert_eq!(without_time(&line), "switch 01M3QRK1AP7M: planned; instance 0 wakes on gpu-box after releasing 1 instance(s)");
        // `chosen` is the generation of the first candidate, which is the pick.
        let route = json!({"event": "router_selection", "deployment": "01M3QRCJJ0MXR2F8ZAM3", "chosen": 1,
            "candidates": [{"host": "gpu-box", "instance": 0, "generation": 1}]});
        assert_eq!(without_time(&format_event(&route).unwrap()), "request 01M3QRCJJ0MX -> gpu-box instance 0");
        assert_eq!(format_event(&json!({"event": "unknown_thing"})), None);
    }

    // T02 (ADR 0021)
    #[test]
    fn banner_and_shutdown_in_text() {
        let banner_value = json!({"role": "standalone", "ready": true, "version": "0.1.0",
            "inference": "0.0.0.0:8443", "inference_auth": "api_key", "management": "127.0.0.1:7443",
            "state_dir": "/s", "credentials": "/s/identity/credentials"});
        assert_eq!(
            banner_text(&banner_value),
            "mllm 0.1.0 standalone ready\n\n  Inference     0.0.0.0:8443 (API key required)\n  Management    127.0.0.1:7443\n  State         /s\n  Credentials   /s/identity/credentials\n"
        );
        let stopped_value = json!({"role": "standalone", "stopped": true, "engines": "retained",
            "drain": {"drained": true, "in_flight_at_close": 0, "cancelled": 0}});
        assert_eq!(stopped_text(&stopped_value), "mllm standalone stopped: drained, 0 requests in flight; engines kept running\n");
    }
}
```

In `crates/mllm-cli/tests/role_shutdown.rs`, add:

```rust
// T02 (ADR 0021): a role prints text with `--format text` even when piped,
// and JSON lines when piped without it.
#[test]
fn a_role_prints_text_when_asked_and_json_when_piped() {
    let installation = Installation::new();
    let mut command = installation.command();
    command.args(["start", "standalone", "--format", "text"]);
    let (line, _) = run_until_ready(&mut command, |line| line.starts_with("mllm ") && line.ends_with(" standalone ready"));
    assert!(line.contains("standalone ready"), "{line}");

    let mut command = installation.command();
    command.args(["start", "standalone"]);
    let (line, _) = run_until_ready(&mut command, |line| line.contains("\"role\":\"standalone\""));
    let banner: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(banner["ready"], true);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-cli --lib role_text && cargo test -p mllm-cli --test role_shutdown a_role_prints_text`
Expected: FAIL to compile / FAIL.

- [ ] **Step 3: Implement**

Put above the tests in `crates/mllm-cli/src/role_text.rs`:

```rust
//! ADR 0021: the text form of role output — the ready banner, one line per
//! event, and the shutdown summary. JSON mode prints the objects unchanged.

use serde_json::Value;

use crate::detail::Detail;
use crate::output::OutputFormat;
use mllm_domain::role_log::{self, Mode};

/// Set the role output mode and install the event formatter. Call once,
/// before the role prints anything.
pub fn install(format: OutputFormat) {
    role_log::set_mode(match format {
        OutputFormat::Text => Mode::Text,
        OutputFormat::Json => Mode::Json,
    });
    role_log::set_formatter(format_event);
}

fn short(id: &Value) -> String {
    id.as_str().map(|s| s.chars().take(12).collect()).unwrap_or_default()
}

fn clock() -> String {
    // Local wall-clock time; the journal adds its own stamps in JSON mode.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// The text of one event, prefixed with local time; `None` for events
/// without a sentence (the sink then prints its key=value fallback).
pub fn format_event(value: &Value) -> Option<String> {
    let text = match value["event"].as_str()? {
        "switch" => format!(
            "switch {}: {}; {}",
            short(&value["switch_id"]),
            value["phase"].as_str().unwrap_or("").to_lowercase(),
            crate::detail::one_line(value["detail"].as_str().unwrap_or(""))
        ),
        "router_selection" => {
            // The ranking's first candidate is the pick; `chosen` is its generation.
            let pick = &value["candidates"][0];
            format!(
                "request {} -> {} instance {}",
                short(&value["deployment"]),
                pick["host"].as_str().unwrap_or("?"),
                pick["instance"]
            )
        }
        "router_failover" => format!(
            "request {} failed over: {}",
            short(&value["deployment"]),
            crate::detail::one_line(value["reason"].as_str().unwrap_or("another instance was chosen"))
        ),
        _ => return None,
    };
    Some(format!("{} {text}", clock()))
}

pub(crate) fn banner_text(value: &Value) -> String {
    let role = value["role"].as_str().unwrap_or("role");
    let version = value["version"].as_str().unwrap_or(env!("CARGO_PKG_VERSION"));
    let inference = value["inference"].as_str().map(|bind| match value["inference_auth"].as_str() {
        Some("none") => format!("{bind} (no API key)"),
        _ => format!("{bind} (API key required)"),
    });
    let s = |key: &str| value[key].as_str().map(str::to_owned);
    Detail::new(format!("mllm {version} {role} ready"))
        .row_opt("Inference", inference)
        .row_opt("Management", s("management"))
        .row_opt("Bootstrap", s("bootstrap"))
        .row_opt("Control", s("control"))
        .row_opt("Ingress", s("ingress"))
        .row_opt("Host ID", s("host_id"))
        .row_opt("State", s("state_dir"))
        .row_opt("Credentials", s("credentials"))
        .render()
}

pub(crate) fn stopped_text(value: &Value) -> String {
    let role = value["role"].as_str().unwrap_or("role");
    let drain = &value["drain"];
    let how = if drain["forced"] == true { "forced" } else if drain["drained"] == true { "drained" } else { "stopped" };
    let in_flight = drain["in_flight_at_close"].as_u64().unwrap_or(0);
    let engines = if value["engines"] == "retained" { "; engines kept running" } else { "" };
    format!("mllm {role} stopped: {how}, {in_flight} requests in flight{engines}\n")
}

/// The ready banner in the current mode (stdout).
pub fn banner(value: &Value) -> String {
    match role_log::mode() {
        Mode::Text => banner_text(value),
        Mode::Json => format!("{value}\n"),
    }
}

/// The shutdown summary in the current mode (stdout).
pub fn stopped(value: &Value) -> String {
    match role_log::mode() {
        Mode::Text => stopped_text(value),
        Mode::Json => format!("{value}\n"),
    }
}
```

`libc` is already a dependency of `mllm-cli`.

In `crates/mllm-cli/src/main.rs`, right after `format` is resolved:

```rust
    if role {
        mllm_cli::role_text::install(format);
    }
```

Standalone banner (`main.rs:431-435`) becomes:

```rust
    print!(
        "{}",
        mllm_cli::role_text::banner(&serde_json::json!({
            "role": "standalone", "ready": true, "version": env!("CARGO_PKG_VERSION"),
            "inference": inference_address.to_string(),
            "inference_auth": if matches!(inference_auth, exposure::InferenceAuth::None) { "none" } else { "api_key" },
            "management": management_address.to_string(),
            "state_dir": state_dir, "credentials": roles::credentials_path(state_dir),
        }))
    );
```

(`exposure::InferenceAuth::None` is the no-key case). Standalone shutdown (`main.rs:459-470`): wrap the existing `json!({...})` in `mllm_cli::role_text::stopped(&…)` and `print!` it.

Server banner (`remote_roles.rs:760-767`): keep the object's current keys, add `"ready": true, "version": env!("CARGO_PKG_VERSION"), "bootstrap": config.bootstrap.to_string(), "control": config.control.to_string()`, and print with `print!("{}", crate::role_text::banner(&value))`. Host banner (`remote_roles.rs:1116-1126`): replace the `println!` with the same call on `json!({"role": "host", "ready": true, "version": env!("CARGO_PKG_VERSION"), "state_dir": config.state_dir, "ingress": <the bind or null>, "credentials": <the identity file>})`.

Server and host shutdown results are returned from `remote_roles::execute` and printed by `emit` in `main.rs`: in the role branch of `main.rs`, print them with `print!("{}", mllm_cli::role_text::stopped(&value))` instead of `emit` when `invocation.command` is `Start(Role::Server | Role::Host)`.

Update the banner matchers: in `crates/mllm-cli/tests/role_shutdown.rs` replace `line.starts_with("standalone ready")` with `line.contains("\"role\":\"standalone\"")` and `line.starts_with("host ready")` with `line.contains("\"role\":\"host\"")` (tests pipe stdout, so the banner is JSON). Do the same at `crates/mllm-cli/tests/engine_exit.rs:420` (read the surrounding helper: it matches a line prefix; switch it to a substring match on `"role":"standalone"`). Where a test reads `state_dir` or a listener address from the old text banner, read it from the JSON banner's fields instead.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-cli --lib role_text && cargo test -p mllm-cli --test role_shutdown && cargo test -p mllm-cli --test engine_exit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat: print role banners and events as text on a terminal"
```

---

### Task 8: Library and role notices go through the sink

**Files:**
- Modify (event lines, keep their JSON exactly, route through `mllm_domain::role_log::event`):
  - `crates/mllm-controller/src/switching.rs:274` (switch)
  - `crates/mllm-controller/src/coordinator/worker.rs:2750` (residency_blocked)
  - `crates/mllm-agent/src/native_execution/saver_source.rs:128`
  - `crates/mllm-agent/src/native_execution/residency.rs:345`
  - `crates/mllm-router/src/balance.rs:386, 548, 576`
  - `crates/mllm-launchers/src/native_observation.rs:41, 64, 567`
- Modify (plain text lines, route through `role_log::notice(Level::Warning | Level::Notice, …)`):
  - `crates/mllm-controller/src/request_leases.rs:155`, `engine_exit.rs:171, 182`, `switching.rs:272, 843`, `agent_sessions.rs:868, 1074, 1099, 1407, 1433, 1545, 1553, 1564`
  - `crates/mllm-agent/src/sources.rs:250` (the default log closure), `session.rs:429`
  - `crates/mllm-router/src/chat.rs:459`
  - `crates/mllm-adapters/src/vllm/initialize.rs:152`, `sglang/initialize.rs:152`
  - `crates/mllm-cli/src/main.rs` role-path `eprintln!` lines (`warning: {notice}` for config notices, deprecation warnings, the debug-engine-logs line), `managed_runtime.rs:18`, `exposure.rs:57`, `remote_roles.rs:1097, 1296, 1363`, `local_role.rs:345`, `shutdown.rs:116`, `roles.rs:458`
- Leave unchanged: `crates/mllm-controller/src/operations.rs:918` (`DEBUG op …`): check whether it is behind a debug flag; if it prints unconditionally in normal runs, route it through `notice(Level::Notice, …)`; if it is test-only or flag-gated, leave it. Leave every print inside `#[cfg(test)]` code, and the command-path lines `Request identity: …` (`client.rs:90`, `revoke.rs:88`, `drain.rs:165`) and `Waiting for …` (`client.rs:403`) — those belong to commands, not roles.

**Interfaces:**
- Consumes: `mllm_domain::role_log::{event, notice, Level}` (Task 2).

- [ ] **Step 1: Write the failing test**

Add to `crates/mllm-cli/tests/role_shutdown.rs`:

```rust
// T02 (ADR 0021): every line a role writes while piped is one JSON object.
#[test]
fn a_piped_role_writes_only_json_lines() {
    let installation = Installation::new();
    let mut command = installation.command();
    command.args(["start", "standalone"]);
    let (_, stderr) = run_until_ready(&mut command, |line| line.contains("\"role\":\"standalone\""));
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "not a JSON line: {line}"
        );
    }
}
```

If `run_until_ready` does not return the role's stderr text in its second value, read how `stderr_of` is used above it and collect stderr the same way.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mllm-cli --test role_shutdown a_piped_role_writes_only_json_lines`
Expected: FAIL on a text line (for example a deprecation or config warning, or `engine build limits`), or PASS if this installation prints no text line at startup. If it passes, add `.env("MLLM_STANDALONE_INFERENCE_ADDR", "127.0.0.1:0")` to the command so the deprecation warning prints, and confirm it fails.

- [ ] **Step 3: Route every listed site through the sink**

For an event site, replace

```rust
eprintln!("{}", serde_json::json!({ ... }));
```

with

```rust
mllm_domain::role_log::event(serde_json::json!({ ... }));
```

keeping the object literally the same. For a text site, replace `eprintln!("…", args)` with

```rust
mllm_domain::role_log::notice(mllm_domain::role_log::Level::Warning, &format!("…", args));
```

using `Level::Warning` for lines that report a problem (refused, not recorded, ended, no heartbeat, stays charged, could not start, not accepted) and `Level::Notice` for informational ones (engine build limits, resumed, reconnecting, full engine logs enabled). A line that already starts with `warning: ` drops that prefix, since the sink adds it. In `crates/mllm-agent/src/sources.rs:250`, the default closure becomes `Arc::new(|line| mllm_domain::role_log::notice(mllm_domain::role_log::Level::Notice, line))`.

Add `mllm-domain` to any listed crate's `Cargo.toml` that lacks it (all six have it today).

- [ ] **Step 4: Run to verify**

Run: `cargo test -p mllm-cli --test role_shutdown && cargo test -p mllm-controller -p mllm-agent -p mllm-router -p mllm-adapters -p mllm-launchers --no-fail-fast 2>&1 | grep -E "^test result|FAILED"`
Expected: PASS. A library test that asserted a text line on stderr now sees the JSON notice form (the default mode is JSON); update such an assertion to the JSON form `{"level":"warning","message":"…"}`.

Then confirm no print is left outside tests:

Run: `grep -rnE 'eprintln!|println!' crates/mllm-controller/src crates/mllm-agent/src crates/mllm-router/src crates/mllm-adapters/src crates/mllm-launchers/src --include=*.rs | grep -vE '/tests?/|tests\.rs'`
Expected: only lines inside `#[cfg(test)]` modules (check each remaining hit) and `operations.rs:918` if it is gated.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "refactor: route role events and notices through one sink"
```

---

### Task 9: Live harness

**Files:**
- Modify: `scripts/live/matrix/discrete_gpu.sh:82`
- Modify: any harness call that parses a command's JSON without `--json`

**Interfaces:** none.

- [ ] **Step 1: Fix the banner match**

In `scripts/live/matrix/discrete_gpu.sh:82`, replace `grep -q "standalone ready" "$log"` with `grep -q '"role":"standalone"' "$log"` (the role's output goes to a file, so it is JSON).

- [ ] **Step 2: Find harness command calls without `--json`**

Run: `grep -rnE '\$(MLLM|M|BIN|mllm)[^|]*\b(deploy|park|stop|start deployment|delete|invite|join|init|engine|validate|inspect|status|list|drain|revoke|prune|preinitialize)\b' scripts/live/matrix | grep -v -- '--json' | grep -v -- '--format json'`

For each hit whose output is parsed (piped to `python3`, `jq`, `json.loads`, or captured into a variable read as JSON), add `--json`. A hit whose output is only shown to the operator stays as it is.

- [ ] **Step 3: Check the scripts still parse**

Run: `bash -n scripts/live/matrix/*.sh scripts/live/matrix/rows/*.sh && python3 -m py_compile scripts/live/matrix/*.py && shellcheck scripts/live/matrix/discrete_gpu.sh`
Expected: no output.

- [ ] **Step 4: Commit**

```bash
git add scripts/live/matrix
git commit -m "test: live harness matches the JSON role banner and asks for JSON"
```

---

### Task 10: Operator documentation

**Files:**
- Modify: `docs/operations/configuration.md:300-325`
- Modify: `docs/operations/install.md` (services section: journal format)
- Modify: `docs/operations/release-notes-0.1.0.md`

**Interfaces:** none.

- [ ] **Step 1: configuration.md**

Replace the paragraph that starts "`--format json` (or `--json`) prints the same as" with:

```markdown
`--format json` (or `--json`) prints the JSON result instead:
`{"role", "document", "settings": [{"path", "value", "source"}]}`. The flags a
role start takes (`--deep-park`, `--listen`, ...) are not options of `config
show`; their variables are read from the environment, and `--set` stands in
for them.
```

and in "Command options that are not settings", replace "`--format table|json` (`--json`)" with "`--format text|json` (`--json`)". Add after that paragraph:

```markdown
Commands print text (tables, or a summary with details) unless `--format json`
is given, even when their output is piped. `start server`, `start host` and
`start standalone` print text when their output is a terminal and one JSON
object per line otherwise, so the journal and log files are JSON; `--format`
overrides that either way.
```

- [ ] **Step 2: install.md**

In the section that introduces the systemd units, add:

```markdown
Services log one JSON object per line, because their output is not a
terminal. Read them as they come with `journalctl -u mllm-host -o cat`, or
pretty-printed with `journalctl -u mllm-host -o cat | jq`. For text in the
journal, add `--format text` to `ExecStart=` in a drop-in.
```

- [ ] **Step 3: Release notes**

Under the release's feature list in `docs/operations/release-notes-0.1.0.md`, add:

```markdown
- Commands print readable text by default: tables for lists, and a summary
  with key-value details for single results. Add `--json` for scripts. Roles
  print text in a terminal and JSON lines to the journal and log files.
```

- [ ] **Step 4: Commit**

```bash
git add docs/operations
git commit -m "docs: describe text output and JSON on request"
```

---

### Task 11: Guide examples from a real run, status, verification

**Files:**
- Modify: `README.md`, `docs/guide/deploy.md`, `docs/guide/engines.md`, `docs/guide/several-machines.md`, `docs/guide/one-machine.md`, `docs/guide/install.md`, and any other `docs/guide/*.md` whose `text` blocks show command or role output
- Modify: `docs/runbooks/f2-current-status.md`

**Interfaces:** none.

- [ ] **Step 1: Run the full local check set**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4
cargo test --workspace --all-targets --locked
scripts/verify-packaging.sh
scripts/test-install.sh
(cd site && MLLM_PUBLISH=1 npm run check)
```

Expected: all pass. Fix any failure before continuing.

- [ ] **Step 2: Capture real output for the guides**

Build the branch (`cargo build --release -p mllm-cli`) and, on the maintainer laptop with a real engine, run each command the guides show (standalone quickstart; `engine add`; `deploy` with and without `--wait`; `park`; `status`; `list`; and, with a server and host A, `init`, `invite`, `join`, `validate`), in a terminal so roles print text. Save each output in the session scratchpad.

- [ ] **Step 3: Replace every example**

Find every output block: `grep -nE '^\{"|^standalone ready|^host ready' README.md docs/guide/*.md`. Replace each with the captured text, generalizing only the values the guides already generalize (`/home/me`, `gpu-box`, `my-model`, IDs of the same shape). Where prose describes the JSON output (for example `docs/guide/engines.md`, "prints `"published":"published"`"), rewrite it to the text (`Published   yes`) and mention `--json` once for scripts.

- [ ] **Step 4: Site check**

Run: `(cd site && MLLM_PUBLISH=1 npm run check)`
Expected: pass.

- [ ] **Step 5: Status runbook**

Add at the top of `docs/runbooks/f2-current-status.md`:

```markdown
## Terminal output — 2026-09-29

ADR 0021 is implemented: commands print tables or a summary with key-value
details unless `--json` is given, and roles print text on a terminal and one
JSON object per line to the journal, files and pipes. JSON results and event
fields are unchanged; tests and the live harness ask for JSON where they
parse it. The guides show output from a real run. CPU and Fake-engine tests
are not qualification.
```

- [ ] **Step 6: Commit and open the pull request**

```bash
git add README.md docs
git commit -m "docs: show real text output in the guides"
git push -u origin feat/terminal-output
gh pr create --repo edurdias/mllm --base main --title "Terminal output by default, JSON on request"
```

The PR body lists what changed, the verification commands and results, and the sentence "CPU and Fake-engine tests are not native engine qualification." No attribution lines.

- [ ] **Step 7: Live check**

With the merged build: standalone on the laptop in a terminal (text banner, text switch and request lines, text shutdown), the same with `2>role.log` (JSON lines only), and a server with hosts A and B started by the harness (`scripts/live/matrix/roles.sh`), confirming `selections.py` reads the `router_selection` lines. Record the result in the status runbook entry.
