//! Owner decision 2026-09-25: commands that read records print an aligned,
//! human-readable table by default (terminal or not), like the docker CLI.
//! `--format json` prints the command's JSON result unchanged. Only the
//! rendering lives here; every view is built from that same JSON result, so
//! a table can never show something the JSON does not carry. Wide or nested
//! detail (latency distributions, fingerprints, development-control marks)
//! stays in the JSON.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::grammar::{Command, ListResource};

/// Host id to host name, from the server's host inventory.
pub type HostNames = BTreeMap<String, String>;

/// The record views that print as a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `list hosts`
    Hosts,
    /// `list deployments`
    Deployments,
    /// `list engines`: every host's published runtime profiles.
    Engines,
    /// `status deployment`
    Status,
    /// `engine list`: this machine's runtime profiles.
    LocalEngines,
    /// `engine detect`
    Detected,
}

impl View {
    /// The table view of a command, or `None` when it prints its JSON result
    /// (mutations, `inspect`, `validate`, `prune`, `drain`, `revoke`).
    pub fn of(command: &Command) -> Option<Self> {
        match command {
            Command::List { resource } => Some(match resource {
                ListResource::Hosts => View::Hosts,
                ListResource::Deployments => View::Deployments,
                ListResource::Engines => View::Engines,
            }),
            Command::Status { .. } => Some(View::Status),
            Command::EngineList => Some(View::LocalEngines),
            Command::EngineDetect { .. } => Some(View::Detected),
            _ => None,
        }
    }

    /// Views whose records name hosts by id only; the caller resolves names
    /// from the host inventory (best effort) before rendering.
    pub fn needs_host_names(self) -> bool {
        matches!(self, View::Deployments | View::Status)
    }
}

/// The host names in a `GET /management/v1/hosts` answer.
pub fn host_names(inventory: &Value) -> HostNames {
    inventory["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|host| {
            let id = host["host_id"].as_str()?;
            let name = host["name"].as_str().filter(|name| !name.is_empty())?;
            Some((id.to_owned(), name.to_owned()))
        })
        .collect()
}

/// Render `value` (the command's JSON result) as the view's table.
pub fn render(view: View, value: &Value, names: &HostNames) -> String {
    match view {
        View::Hosts => hosts(value),
        View::Deployments => deployments(value, names),
        View::Engines => engines(value),
        View::Status => status(value, names),
        View::LocalEngines => local_engines(value),
        View::Detected => detected(value),
    }
}

/// Columns left-aligned, separated by three spaces, upper-case headers, no
/// trailing blanks. An empty result prints the header line only.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(i) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut text = String::new();
        for (i, cell) in cells.enumerate() {
            if i > 0 {
                text.push_str("   ");
            }
            text.push_str(cell);
            let pad = widths[i].saturating_sub(cell.chars().count());
            text.push_str(&" ".repeat(pad));
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    line(&mut headers.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str).take(headers.len()));
    }
    out
}

/// Bytes in GiB with one decimal (`12.5 GiB`).
pub fn gib(bytes: i64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// Milliseconds as seconds (`600s`, `1.5s`).
pub fn seconds(ms: i64) -> String {
    if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// A host by name; its id when the inventory has no name for it.
pub fn host_label(id: &str, names: &HostNames) -> String {
    names.get(id).cloned().unwrap_or_else(|| id.to_owned())
}

/// One line of text: control characters (a multi-line error) become spaces
/// so they cannot break the alignment.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A scalar cell: strings as they are, numbers as written, `-` for absent.
pub(crate) fn text(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => clean(s),
        Value::Bool(b) => yes_no(*b),
        Value::Array(items) => list(items),
        other => clean(&other.to_string()),
    }
}

fn yes_no(flag: bool) -> String {
    if flag { "yes" } else { "no" }.into()
}

/// A list cell: names joined with commas, `-` when empty.
fn list(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::String(s) => clean(s),
            Value::Object(o) => o
                .get("name")
                .or_else(|| o.get("deployment"))
                .or_else(|| o.get("id"))
                .and_then(Value::as_str)
                .map(clean)
                .unwrap_or_else(|| clean(&item.to_string())),
            other => clean(&other.to_string()),
        })
        .collect();
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(",")
    }
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// SPEC §4.1 / ADR 0017: a host's standing, most severe first.
fn host_state(host: &Value) -> &'static str {
    let session = &host["session"];
    if host["revoked"] == true {
        "revoked"
    } else if host["online"] != true {
        "offline"
    } else if session["unresponsive"] == true {
        "unresponsive"
    } else if session["drain_pending"] == true {
        "draining"
    } else if session["reconciled"] == false {
        "reconciling"
    } else {
        "online"
    }
}

fn hosts(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|host| {
            let id = host["host_id"].as_str().unwrap_or("-");
            let name = host["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or(id);
            let domains = host["session"]["domains"].as_array();
            let memory = match domains.filter(|d| !d.is_empty()) {
                Some(domains) => {
                    let sum = |key: &str| domains.iter().filter_map(|d| number(&d[key])).sum();
                    format!(
                        "{} / {}",
                        gib(sum("available_bytes")),
                        gib(sum("capacity_bytes"))
                    )
                }
                None => "-".into(),
            };
            let engines = host["session"]["profiles"]
                .as_array()
                .map(|p| list(p))
                .unwrap_or_else(|| "-".into());
            vec![
                clean(name),
                host_state(host).into(),
                yes_no(host["eligible"] == true),
                text(&host["binary_version"]),
                text(&host["compatibility"]),
                memory,
                engines,
            ]
        })
        .collect();
    table(
        &[
            "NAME",
            "STATE",
            "ELIGIBLE",
            "VERSION",
            "COMPATIBILITY",
            "MEMORY (FREE / TOTAL)",
            "ENGINES",
        ],
        &rows,
    )
}

/// The distinct hosts a deployment's instances are placed on, by name.
pub(crate) fn instance_hosts(deployment: &Value, names: &HostNames) -> String {
    let mut seen: Vec<String> = Vec::new();
    for instance in deployment["instances"].as_array().into_iter().flatten() {
        if let Some(id) = instance["host_id"].as_str() {
            let label = host_label(id, names);
            if !seen.contains(&label) {
                seen.push(label);
            }
        }
    }
    if seen.is_empty() {
        "-".into()
    } else {
        seen.join(",")
    }
}

pub(crate) fn ready(deployment: &Value) -> String {
    format!(
        "{}/{}",
        number(&deployment["ready_instances"]).unwrap_or(0),
        number(&deployment["desired_instances"]).unwrap_or(0)
    )
}

// Owner decision 2026-09-26: KIND is always "model" today and DESIRED is
// operator intent, not what is happening; both stay out of the table. STATE
// alone is the store's already-derived `observed_state` (SPEC §§6.1, 6.4;
// see the comment on `OBSERVED_STATE` in capyctl-store's snapshot query), which
// reads the desired/observed combination for us: a crash reads `failed`, a
// stop in flight reads `stopping`, a park in flight reads `parking`, and so
// on, never a bare `stopped` that could mean either intent or failure.
// `--format json` is unchanged: both fields stay in the JSON result.
fn deployments(value: &Value, names: &HostNames) -> String {
    let rows: Vec<Vec<String>> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            vec![
                text(&d["name"]),
                text(&d["observed_state"]),
                ready(d),
                text(&d["revision"]),
                instance_hosts(d, names),
            ]
        })
        .collect();
    table(&["NAME", "STATE", "READY", "REVISION", "HOSTS"], &rows)
}

/// A startup reservation: on a discrete GPU the card's figure with the host
/// RAM beside it (ADR 0019 §3), else the one pool's.
pub(crate) fn startup(startup: &Value) -> String {
    match (
        number(&startup["device_bytes"]),
        number(&startup["host_bytes"]),
    ) {
        (Some(device), Some(host)) => format!("{} (+{} RAM)", gib(device), gib(host)),
        _ => number(&startup["bytes"])
            .map(gib)
            .unwrap_or_else(|| "-".into()),
    }
}

pub(crate) fn operation(op: &Value) -> String {
    if !op.is_object() {
        return "-".into();
    }
    let mut out = format!("{} {}", text(&op["kind"]), text(&op["state"]));
    if let Some(code) = op["error_code"].as_str() {
        out.push_str(&format!(" ({})", clean(code)));
    }
    out
}

/// An instance's LAST ERROR: the error it recorded (a placement refusal),
/// else, when its latest operation failed, that failure's code and the first
/// line of its message.
fn last_error(instance: &Value) -> String {
    if let Some(error) = instance["last_error"].as_str().filter(|e| !e.is_empty()) {
        return clean(error);
    }
    let op = &instance["latest_operation"];
    if op["state"] != "failed" {
        return "-".into();
    }
    let message = op["reason"].as_str().and_then(|r| r.lines().next());
    match (op["error_code"].as_str(), message) {
        (Some(code), Some(message)) => clean(&format!("{code}: {message}")),
        (Some(code), None) => clean(code),
        (None, Some(message)) => clean(message),
        (None, None) => "failed".into(),
    }
}

fn status(value: &Value, names: &HostNames) -> String {
    let d = value;
    let startup = startup(&d["startup"]);
    let initialize = number(&d["timeouts"]["initialize_ms"])
        .map(seconds)
        .unwrap_or_else(|| "-".into());
    // See the comment on `deployments` above: KIND and DESIRED stay out of
    // the table, and STATE (the derived `observed_state`) says on its own
    // whether an instance is stopped by intent, still starting, or failed.
    let mut out = table(
        &[
            "NAME",
            "STATE",
            "READY",
            "REVISION",
            "STARTUP",
            "INITIALIZE",
            "LAST OPERATION",
        ],
        &[vec![
            text(&d["name"]),
            text(&d["observed_state"]),
            ready(d),
            text(&d["revision"]),
            startup,
            initialize,
            operation(&d["latest_operation"]),
        ]],
    );
    // Found live 2026-10-03: a start that waits (for memory, a measurement)
    // or a checkpoint the host cannot measure says why here, not only in
    // the JSON.
    let mut notes = Vec::new();
    let op = &d["latest_operation"];
    if op["state"] == "pending" {
        if let Some(reason) = op["reason"].as_str().and_then(|r| r.lines().next()) {
            notes.push(format!("Waiting     {}: {}", operation(op), clean(reason)));
        }
    }
    // ADR 0014 §7 (found live on a 16 GB card): a checkpoint that measured
    // to its digest but whose weights do not resolve the revision says why,
    // not that it could not be measured.
    if let Some(diagnostic) = d["checkpoint_digest"]["diagnostic"].as_str() {
        notes.push(if d["checkpoint_digest"]["state"] == "unusable" {
            format!(
                "Checkpoint  unusable: the memory does not resolve with the measured weights ({}); deploy a corrected configuration",
                clean(diagnostic)
            )
        } else {
            format!("Checkpoint  could not be measured ({})", clean(diagnostic))
        });
    }
    // Found live 2026-10-04: a declared fingerprint that is a weight file's
    // hash, not CapyCTL's checkpoint digest, says which value is expected.
    let digest = &d["checkpoint_digest"];
    if digest["state"] == "mismatch" {
        if let (Some(declared), Some(measured)) =
            (digest["expected"].as_str(), digest["digest"].as_str())
        {
            notes.push(format!(
                "Checkpoint  mismatch: declared {}, measured {}. model.content_fingerprint is \
                 CapyCTL's checkpoint digest (over every file under the model directory), not \
                 a file hash: set it to the measured value, or leave it out",
                clean(declared),
                clean(measured)
            ));
        }
    }
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            out.push_str(&note);
            out.push('\n');
        }
    }
    let instances: Vec<Vec<String>> = d["instances"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            let host = i["host_id"]
                .as_str()
                .map(|id| host_label(id, names))
                .unwrap_or_else(|| "-".into());
            let devices = match &i["devices"] {
                Value::Object(o) if o.is_empty() => "-".into(),
                Value::Object(o) => clean(&Value::Object(o.clone()).to_string()),
                other => text(other),
            };
            vec![
                text(&i["index"]),
                host,
                text(&i["observed_state"]),
                text(&i["lifecycle"]),
                devices,
                last_error(i),
            ]
        })
        .collect();
    out.push('\n');
    out.push_str(&table(
        &[
            "INSTANCE",
            "HOST",
            "STATE",
            "LIFECYCLE",
            "DEVICES",
            "LAST ERROR",
        ],
        &instances,
    ));
    if let Some(engine) = d["engine"].as_str() {
        out.push_str(&format!("\nEngine  {engine}\n"));
    } else if d["installation"].is_object() {
        let i = &d["installation"];
        out.push_str(&format!(
            "\nEngine  {} {} ({})\n",
            text(&i["profile"]),
            text(&i["version"]),
            text(&i["executable"])
        ));
        if let Some(note) = d["engine_note"].as_str() {
            out.push_str(&format!("note: {note}\n"));
        }
    }
    if let Some(line) = parsers(&d["parsers"]) {
        // Under the engine line when there is one, else after a blank line.
        if !(d["engine"].is_string() || d["installation"].is_object()) {
            out.push('\n');
        }
        out.push_str(&line);
    }
    // ADR 0014 amendment A14: a hybrid model's state cache holds fewer
    // running requests than the deployment asked for.
    if let Some(limit) = d["context"]["running_limit"].as_u64() {
        out.push_str(&format!(
            "Running limited to {limit} request{} by the state cache\n",
            if limit == 1 { "" } else { "s" }
        ));
    }
    if let Some(line) = streams(&d["context"]["streams"]) {
        out.push_str(&line);
    }
    out
}

/// ADR 0023 §4 (amended 2026-10-03): the requests a TensorFold launch decodes
/// together, one line, absent for other engines.
fn streams(s: &Value) -> Option<String> {
    let source = s["source"].as_str()?;
    let whose = match source {
        "declared" => "engine_config.max_concurrent_requests",
        "default" => "CapyCTL default",
        "extra_args" => "from extra_args",
        "host_fixed" => "from host-fixed args",
        other => other,
    };
    let reason = s["reason"]
        .as_str()
        .map(|r| format!(": {}", clean(r)))
        .unwrap_or_default();
    Some(match s["count"].as_u64() {
        Some(1) => format!("Streams 1 request at a time ({whose}){reason}\n"),
        Some(n) => format!("Streams up to {n} requests decoded together ({whose}){reason}\n"),
        None => format!(
            "Streams set by {}{reason}\n",
            whose.trim_start_matches("from ")
        ),
    })
}

/// ADR 0024: the parsers a launch passes, one line, absent for an engine
/// without a parser setting.
fn parsers(p: &Value) -> Option<String> {
    if !p.is_object() {
        return None;
    }
    let one = |c: &Value| match (c["name"].as_str(), c["source"].as_str()) {
        (Some(name), _) => clean(name),
        (None, Some("extra_args")) => "from extra_args".into(),
        (None, Some("host_fixed")) => "from host-fixed args".into(),
        (None, Some("on_host")) => "chosen on the host".into(),
        (None, Some("off")) => "off".into(),
        _ => "none".into(),
    };
    let family = p["family"]
        .as_str()
        .map(|f| format!(" (model family {})", clean(f)))
        .unwrap_or_default();
    Some(format!(
        "Parsers tool calls: {}, reasoning: {}{family}\n",
        one(&p["tool_call"]),
        one(&p["reasoning"])
    ))
}

fn engines(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["engines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let host = e["host"]
                .as_str()
                .filter(|n| !n.is_empty())
                .or_else(|| e["host_id"].as_str())
                .unwrap_or("-");
            let state = if e["retiring"] == true {
                "retiring"
            } else if e["online"] == true {
                "online"
            } else {
                "offline"
            };
            vec![
                clean(host),
                text(&e["profile"]),
                text(&e["engine"]),
                text(&e["version"]),
                text(&e["custom"]),
                text(&e["deep_park"]),
                state.into(),
                text(&e["deployments"]),
            ]
        })
        .collect();
    table(
        &[
            "HOST",
            "PROFILE",
            "ENGINE",
            "VERSION",
            "CUSTOM",
            "DEEP PARK",
            "STATE",
            "DEPLOYMENTS",
        ],
        &rows,
    )
}

fn local_engines(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["engines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            vec![
                text(&e["profile"]),
                text(&e["source"]),
                text(&e["engine"]),
                text(&e["version"]),
                text(&e["custom"]),
                text(&e["deep_park"]),
                text(&e["published"]),
                text(&e["deployments"]),
            ]
        })
        .collect();
    table(
        &[
            "PROFILE",
            "SOURCE",
            "ENGINE",
            "VERSION",
            "CUSTOM",
            "DEEP PARK",
            "PUBLISHED",
            "DEPLOYMENTS",
        ],
        &rows,
    )
}

fn detected(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            vec![
                text(&c["engine"]),
                text(&c["version"]),
                text(&c["custom"]),
                text(&c["env"]),
                text(&c["source"]),
            ]
        })
        .collect();
    table(
        &["ENGINE", "VERSION", "CUSTOM", "ENVIRONMENT", "SOURCE"],
        &rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names() -> HostNames {
        host_names(&json!({"hosts": [
            {"host_id": "01HOSTA", "name": "gpu-a"},
            {"host_id": "01HOSTB", "name": ""},
        ]}))
    }

    #[test]
    fn columns_align_and_lines_carry_no_trailing_blanks() {
        let out = table(
            &["NAME", "STATE"],
            &[
                vec!["a-long-name".into(), "ready".into()],
                vec!["b".into(), "stopped".into()],
            ],
        );
        assert_eq!(
            out,
            "NAME          STATE\na-long-name   ready\nb             stopped\n"
        );
    }

    #[test]
    fn an_empty_result_prints_the_headers_only() {
        assert_eq!(
            render(View::Deployments, &json!([]), &HostNames::new()),
            "NAME   STATE   READY   REVISION   HOSTS\n"
        );
        let hosts = render(View::Hosts, &json!({"hosts": []}), &HostNames::new());
        assert_eq!(hosts.lines().count(), 1);
        assert!(hosts.starts_with("NAME   STATE   ELIGIBLE"));
        let detected = render(
            View::Detected,
            &json!({"candidates": []}),
            &HostNames::new(),
        );
        assert_eq!(
            detected,
            "ENGINE   VERSION   CUSTOM   ENVIRONMENT   SOURCE\n"
        );
    }

    #[test]
    fn host_ids_resolve_to_names_and_fall_back_to_the_id() {
        let names = names();
        assert_eq!(host_label("01HOSTA", &names), "gpu-a");
        // An empty name is no name.
        assert_eq!(host_label("01HOSTB", &names), "01HOSTB");
        assert_eq!(host_label("01HOSTC", &names), "01HOSTC");
        let out = render(
            View::Deployments,
            &json!([{"name": "chat", "kind": "model", "desired_state": "running",
                "observed_state": "ready", "ready_instances": 2, "desired_instances": 2,
                "revision": "3", "instances": [
                    {"index": 0, "host_id": "01HOSTA"},
                    {"index": 1, "host_id": "01HOSTC"},
                    {"index": 2, "host_id": "01HOSTA"}]}]),
            &names,
        );
        let row = out.lines().nth(1).unwrap();
        assert!(row.starts_with("chat   ready"), "{out}");
        assert!(row.ends_with("gpu-a,01HOSTC"), "{out}");
        assert!(row.contains("2/2"), "{out}");
        assert!(!out.contains("01HOSTA"), "{out}");
    }

    // Owner decision 2026-09-26: KIND (always "model") and DESIRED (operator
    // intent, not what is happening) are out of the table; STATE is the
    // store's already-derived `observed_state`, which must still say plainly
    // whether a deployment is stopped by intent, mid-transition, or failed,
    // with no DESIRED column to lean on.
    #[test]
    fn state_alone_distinguishes_stopped_failed_and_stopping_with_no_kind_or_desired_column() {
        let state = |observed: &str| {
            let out = render(
                View::Deployments,
                &json!([{"name": "d", "kind": "model", "desired_state": "ready",
                    "observed_state": observed, "ready_instances": 0, "desired_instances": 1,
                    "revision": "1", "instances": []}]),
                &HostNames::new(),
            );
            let row = out.lines().nth(1).unwrap().to_owned();
            row.split_whitespace().nth(1).unwrap().to_owned()
        };
        assert_eq!(state("stopped"), "stopped");
        assert_eq!(state("failed"), "failed");
        assert_eq!(state("stopping"), "stopping");
        assert_eq!(state("parked"), "parked");
        let out = render(
            View::Deployments,
            &json!([{"name": "d", "kind": "model", "desired_state": "ready",
                "observed_state": "ready", "ready_instances": 1, "desired_instances": 1,
                "revision": "1", "instances": []}]),
            &HostNames::new(),
        );
        assert!(!out.contains("KIND"), "{out}");
        assert!(!out.contains("DESIRED"), "{out}");
    }

    #[test]
    fn units_are_human() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib(3 * (1 << 30) / 2), "1.5 GiB");
        assert_eq!(seconds(600_000), "600s");
        assert_eq!(seconds(1_500), "1.5s");
        let out = render(
            View::Hosts,
            &json!({"hosts": [{"host_id": "01HOSTA", "name": "gpu-a", "revoked": false,
                "online": true, "eligible": true, "binary_version": "0.1.0",
                "compatibility": "supported",
                "session": {"reconciled": true, "domains": [
                    {"available_bytes": 64_i64 << 30, "capacity_bytes": 128_i64 << 30}],
                    "profiles": [{"name": "vllm"}, {"name": "sglang"}]}}]}),
            &HostNames::new(),
        );
        assert!(out.contains("64.0 GiB / 128.0 GiB"), "{out}");
        assert!(out.contains("vllm,sglang"), "{out}");
        assert!(out
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("gpu-a   online   yes"));
    }

    #[test]
    fn host_states_are_named() {
        let row = |host: Value| {
            render(View::Hosts, &json!({"hosts": [host]}), &HostNames::new())
                .lines()
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert!(
            row(json!({"host_id": "01X", "name": null, "revoked": true}))
                .starts_with("01X    revoked")
        );
        assert!(row(json!({"host_id": "01X", "online": false})).contains("offline"));
        assert!(row(json!({"host_id": "01X", "online": true,
            "session": {"drain_pending": true}}))
        .contains("draining"));
    }

    // Owner decision 2026-10-01: status names the engine a deployment runs,
    // and a note when that engine is no longer registered.
    #[test]
    fn status_names_the_pinned_engine_and_a_note() {
        let d = json!({"name": "m", "observed_state": "ready", "instances": [],
            "installation": {"profile": "tensorfold", "version": "0.6.0",
                "executable": "/opt/tf060/bin/tensorfold", "state": "unregistered"},
            "engine_note": "pinned to an engine no longer registered as tensorfold; redeploy to use 0.6.1"});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("\nEngine  tensorfold 0.6.0 (/opt/tf060/bin/tensorfold)\nnote: pinned to an engine no longer registered as tensorfold; redeploy to use 0.6.1\n"),
            "{out}"
        );
    }

    // T14: ADR 0024. Status shows the parsers a launch passes and where they
    // came from; nothing when the engine has no parser setting.
    #[test]
    fn status_shows_the_chosen_parsers() {
        let base = json!({"name": "m", "observed_state": "ready", "instances": []});
        let mut d = base.clone();
        d["parsers"] = json!({"family": "qwen3_5",
            "tool_call": {"name": "qwen3_coder", "source": "model_family"},
            "reasoning": {"name": "qwen3", "source": "model_family"}});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with(
                "\nParsers tool calls: qwen3_coder, reasoning: qwen3 (model family qwen3_5)\n"
            ),
            "{out}"
        );
        d["parsers"] = json!({
            "tool_call": {"source": "extra_args"}, "reasoning": {"source": "off"}});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("\nParsers tool calls: from extra_args, reasoning: off\n"),
            "{out}"
        );
        d["parsers"] = json!({
            "tool_call": {"source": "on_host"}, "reasoning": {"source": "unknown_family"}});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("\nParsers tool calls: chosen on the host, reasoning: none\n"),
            "{out}"
        );
        let out = render(View::Status, &base, &HostNames::new());
        assert!(!out.contains("Parsers"), "{out}");
        d["installation"] = json!({"profile": "vllm", "version": "0.30.0",
            "executable": "/opt/vllm/bin/vllm"});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with(
                "\nEngine  vllm 0.30.0 (/opt/vllm/bin/vllm)\n\
                 Parsers tool calls: chosen on the host, reasoning: none\n"
            ),
            "{out}"
        );
    }

    // T14: ADR 0014 amendment A14. Status says when a hybrid model's state
    // cache limits the running requests; nothing otherwise.
    #[test]
    fn status_shows_a_running_limit_from_the_state_cache() {
        let mut d = json!({"name": "m", "observed_state": "ready", "instances": [],
            "context": {"tokens": 262144, "source": "fitted", "running_limit": 3}});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("\nRunning limited to 3 requests by the state cache\n"),
            "{out}"
        );
        d["context"]["running_limit"] = json!(1);
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("Running limited to 1 request by the state cache\n"),
            "{out}"
        );
        d["context"]
            .as_object_mut()
            .unwrap()
            .remove("running_limit");
        let out = render(View::Status, &d, &HostNames::new());
        assert!(!out.contains("Running limited"), "{out}");
    }

    // ADR 0023 §4 (amended 2026-10-03): status says how many requests a
    // TensorFold launch decodes together and where the count came from.
    #[test]
    fn status_shows_the_streams_a_tensorfold_launch_decodes_together() {
        let mut d = json!({"name": "m", "observed_state": "ready", "instances": [],
            "context": {"tokens": 32768, "source": "declared",
                "streams": {"count": 8, "source": "default"}}});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with("\nStreams up to 8 requests decoded together (CapyCTL default)\n"),
            "{out}"
        );
        d["context"]["streams"] = json!({"count": 1, "source": "extra_args",
            "reason": "TensorFold serves this model family (nemotron_h) one request at a time on CUDA"});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(
            out.ends_with(
                "Streams 1 request at a time (from extra_args): TensorFold serves this \
                 model family (nemotron_h) one request at a time on CUDA\n"
            ),
            "{out}"
        );
        d["context"]["streams"] = json!({"source": "host_fixed"});
        let out = render(View::Status, &d, &HostNames::new());
        assert!(out.ends_with("Streams set by host-fixed args\n"), "{out}");
        d["context"].as_object_mut().unwrap().remove("streams");
        let out = render(View::Status, &d, &HostNames::new());
        assert!(!out.contains("Streams"), "{out}");
    }

    // T02: status on a role with no engine names the command that adds one.
    #[test]
    fn status_on_a_role_with_no_engine_names_engine_add() {
        let out = render(
            View::Status,
            &json!({"name": "chat", "observed_state": "stopped", "instances": [],
                "engine": crate::client::NO_ENGINE}),
            &HostNames::new(),
        );
        assert!(
            out.ends_with("\nEngine  none: run `capyctl engine add <path>`\n"),
            "{out}"
        );
    }

    #[test]
    fn status_prints_the_deployment_then_its_instances() {
        let out = render(
            View::Status,
            &json!({"name": "chat", "kind": "model", "desired_state": "running",
                "observed_state": "failed", "ready_instances": 0, "desired_instances": 1,
                "revision": "1", "startup": {"bytes": 20_i64 << 30},
                "timeouts": {"initialize_ms": 900_000},
                "latest_operation": {"kind": "start", "state": "failed", "error_code": "activation_timeout"},
                "latency": {"tiers": []},
                "instances": [{"index": 0, "host_id": "01HOSTA", "observed_state": "failed",
                    "lifecycle": "stopped", "devices": ["GPU-0"],
                    "last_error": "engine exited\nwith code 1"}]}),
            &names(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("NAME   STATE"), "{out}");
        assert!(lines[1].contains("20.0 GiB"), "{out}");
        assert!(lines[1].contains("900s"), "{out}");
        assert!(
            lines[1].ends_with("start failed (activation_timeout)"),
            "{out}"
        );
        assert_eq!(lines[2], "");
        assert!(lines[3].starts_with("INSTANCE   HOST"), "{out}");
        assert!(lines[4].contains("gpu-a"), "{out}");
        assert!(lines[4].ends_with("engine exited with code 1"), "{out}");
        assert!(!out.contains("tiers"), "latency stays in the JSON");
    }

    // T16 (found live on a 16 GB laptop GPU, 2026-10-03): on a discrete GPU
    // STARTUP is the card's figure, with the host RAM beside it, not their
    // sum; a start waiting for memory says why under the table, and so does
    // a checkpoint the host could not measure.
    #[test]
    fn status_shows_the_cards_startup_and_why_a_start_waits() {
        let out = render(
            View::Status,
            &json!({"name": "fv", "kind": "model", "desired_state": "running",
                "observed_state": "queued", "ready_instances": 0, "desired_instances": 1,
                "revision": "1",
                "startup": {"bytes": 19_838_388_100_i64, "device_bytes": 15_543_420_804_i64,
                    "host_bytes": 4_294_967_296_i64},
                "checkpoint_digest": {"state": "pending", "provisional": true,
                    "diagnostic": "invalid_root"},
                "latest_operation": {"kind": "initialize", "state": "pending",
                    "reason": "gave up: resource or evidence check failed: insufficient resources"},
                "instances": []}),
            &names(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("14.5 GiB (+4.0 GiB RAM)"), "{out}");
        assert!(!out.contains("18.5 GiB"), "{out}");
        assert!(
            out.contains("Waiting     initialize pending: gave up: resource or evidence check failed: insufficient resources"),
            "{out}"
        );
        assert!(
            out.contains("Checkpoint  could not be measured (invalid_root)"),
            "{out}"
        );
    }

    // Found live 2026-10-04: a declared `sha256:` fingerprint that was a weight
    // file's hash was refused `checkpoint_mismatch` with no word of what the
    // field holds. Status names the declared and measured digests and says
    // that the field is CapyCTL's checkpoint digest.
    #[test]
    fn status_explains_a_checkpoint_mismatch() {
        let declared = format!("sha256:{}", "a".repeat(64));
        let measured = format!("sha256:{}", "b".repeat(64));
        let out = render(
            View::Status,
            &json!({"name": "fv", "kind": "model", "desired_state": "running",
                "observed_state": "stopped", "ready_instances": 0, "desired_instances": 1,
                "revision": "1",
                "checkpoint_digest": {"state": "mismatch", "provisional": false,
                    "expected": declared, "digest": measured},
                "instances": []}),
            &names(),
        );
        assert!(out.contains(&format!("declared {declared}")), "{out}");
        assert!(out.contains(&format!("measured {measured}")), "{out}");
        assert!(out.contains("not a file hash"), "{out}");
    }

    // T14 T26 (found live on a 16 GB card): an unusable checkpoint shows the
    // reason its weights do not resolve the revision, not "could not be
    // measured" and not a mismatch.
    #[test]
    fn status_explains_an_unusable_checkpoint() {
        let reason = "unsupported combination at `engine_config.memory.kv_cache`: the derived \
                      KV cache (request minus weights minus margin) is not positive";
        let out = render(
            View::Status,
            &json!({"name": "fv", "kind": "model", "desired_state": "running",
                "observed_state": "stopped", "ready_instances": 0, "desired_instances": 1,
                "revision": "1",
                "checkpoint_digest": {"state": "unusable", "provisional": true,
                    "digest": format!("sha256:{}", "b".repeat(64)), "diagnostic": reason},
                "instances": []}),
            &names(),
        );
        assert!(
            out.contains(&format!(
                "Checkpoint  unusable: the memory does not resolve with the measured weights ({reason})"
            )),
            "{out}"
        );
        assert!(!out.contains("could not be measured"), "{out}");
        assert!(!out.contains("mismatch"), "{out}");
    }

    // T16: an instance whose launch failed shows the failure's code and
    // message in LAST ERROR (the shape `status deployment` returns after a
    // failed launch), not `-`.
    #[test]
    fn a_failed_launch_is_the_instance_last_error() {
        let failed = json!({"error_code": "launch_failed",
            "hint": "the engine exited before it was ready; check the deployment's engine_config",
            "kind": "initialize", "state": "failed",
            "reason": "launch failed: engine launch failed: the engine exited before readiness\nexit 1"});
        let out = render(
            View::Status,
            &json!({"name": "toy", "kind": "model", "desired_state": "ready",
                "observed_state": "failed", "ready_instances": 0, "desired_instances": 1,
                "revision": "1", "latest_operation": failed,
                "instances": [{"index": 0, "host_id": "01HOSTA", "observed_state": "failed",
                    "lifecycle": "active", "devices": [{"id": "gpu0", "sharing": "shared"}],
                    "last_error": null, "latest_operation": failed},
                    {"index": 1, "host_id": "01HOSTA", "observed_state": "ready",
                    "lifecycle": "active", "devices": [],
                    "latest_operation": {"kind": "initialize", "state": "succeeded"}}]}),
            &names(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[4].ends_with(
                "launch_failed: launch failed: engine launch failed: the engine exited before readiness"
            ),
            "{out}"
        );
        assert!(lines[5].ends_with('-'), "{out}");
    }

    #[test]
    fn engine_views_name_hosts_and_flags() {
        let out = render(
            View::Engines,
            &json!({"engines": [{"host_id": "01HOSTA", "host": "gpu-a", "online": true,
                "profile": "vllm", "engine": "vllm", "version": "0.11.0", "custom": false,
                "deep_park": "enabled", "retiring": false, "deployments": ["chat"]}]}),
            &HostNames::new(),
        );
        assert_eq!(
            out.lines().nth(1).unwrap(),
            "gpu-a   vllm      vllm     0.11.0    no       enabled     online   chat"
        );
        let local = render(
            View::LocalEngines,
            &json!({"engines": [{"profile": "local-vllm", "source": "environment",
                "engine": "vllm", "version": "0.11.0", "custom": true,
                "deep_park": null, "published": "published", "deployments": []}]}),
            &HostNames::new(),
        );
        assert!(local
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("local-vllm   environment   vllm"));
        assert!(local
            .lines()
            .nth(1)
            .unwrap()
            .ends_with("yes      -           published   -"));
    }
}
